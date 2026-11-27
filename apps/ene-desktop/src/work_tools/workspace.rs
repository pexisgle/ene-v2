use super::{
    COMMAND_TIMEOUT, FILE_LIMIT, IMAGE_LIMIT, MAX_RESULTS, OUTPUT_LIMIT, SHELL,
    WORKSPACE_SHELL_CAPABILITY, WorkToolError,
    helpers::{Edit, edit, glob, relative, text},
    search::{Browse, Search, max_results},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_fs_ext::OpenOptionsSyncExt as _;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use cap_tempfile::TempFile;
use ene_client_core::is_attachment_image;
use ene_rpc as w;
use parking_lot::Mutex;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Workspace {
    pub(super) root: PathBuf,
    pub(super) dir: Dir,
    edits: Mutex<()>,
    pub(super) shell: Option<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct Request<T> {
    /// Copy this workspace path exactly.
    workspace: String,
    #[serde(flatten)]
    tool: T,
}

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// Path relative to the workspace, using / between components.
    path: String,
    #[serde(default = "first_line")]
    offset: NonZeroUsize,
    #[serde(default = "max_results")]
    #[schemars(range(max = MAX_RESULTS))]
    limit: NonZeroUsize,
}

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// Path relative to the workspace, using / between components.
    path: String,
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// Path relative to the workspace, using / between components.
    path: String,
    #[schemars(length(min = 1))]
    edits: Vec<Edit>,
}

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    #[serde(flatten)]
    browse: Browse,
    pattern: String,
    #[serde(default, rename = "ignoreCase")]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    #[schemars(with = "String")]
    glob: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    #[serde(flatten)]
    browse: Browse,
    pattern: String,
}

#[derive(Deserialize, JsonSchema)]
struct ShellArgs {
    #[schemars(length(min = 1))]
    command: String,
}

enum FileChange {
    Write(String),
    Edit(Vec<Edit>),
}

struct FileTool {
    name: &'static str,
    spec: fn(&Workspace) -> w::ClientCapabilitySpec,
    run: fn(&Workspace, &str) -> Result<Value, WorkToolError>,
}

// Declare each tool's advertised schema and parser together.
macro_rules! file_tools {
    ($prefix:literal; $($name:literal: $args:ty, $description:literal, $run:expr;)+) => {
        pub(crate) const PREFIX: &str = $prefix;
        pub const WORKSPACE_FILE_CAPABILITIES: &[&str] = &[$(concat!($prefix, $name)),+];
        const FILE_TOOLS: &[FileTool] = &[$(FileTool {
            name: concat!($prefix, $name),
            spec: |workspace| workspace.spec::<$args>($name, $description, "{path}"),
            run: |workspace, input| ($run)(workspace, workspace.arguments::<$args>(input)?),
        }),+];
    };
}

file_tools! {
    "work.";
    "read": ReadArgs, "Read text by line or a PNG, JPEG, GIF or WebP image.",
        |workspace: &Workspace, ReadArgs { path, offset, limit }| {
            workspace.read(relative(&path)?, offset, limit.get().min(MAX_RESULTS))
        };
    "write": WriteArgs, "Create or replace a UTF-8 text file. Creates missing parent folders.",
        |workspace: &Workspace, WriteArgs { path, content }| {
            workspace.change(relative(&path)?, FileChange::Write(content))
        };
    "edit": EditArgs, "Apply exact replacements in order. Each oldText must match exactly once. Save only if all replacements succeed.",
        |workspace: &Workspace, EditArgs { path, edits }| {
            workspace.change(relative(&path)?, FileChange::Edit(edits))
        };
    "grep": GrepArgs, "Search UTF-8 files for matching lines. Pattern is a Rust regular expression. Path defaults to the workspace.",
        Workspace::grep;
    "find": FindArgs, "Find paths matching a glob, for example **/*.rs. Path defaults to the workspace.",
        |workspace: &Workspace, FindArgs { browse, pattern }| {
            workspace.search(&browse, Search::Find(&glob(&pattern)?))
        };
    "ls": Browse, "List a directory. Path defaults to the workspace.",
        |workspace: &Workspace, browse| workspace.search(&browse, Search::List);
}

fn first_line() -> NonZeroUsize {
    NonZeroUsize::MIN
}
impl Workspace {
    pub(crate) fn probe() -> Option<Self> {
        let root = match std::env::var_os("ENE_DESKTOP_WORKSPACE") {
            Some(path) if path.is_empty() => return None,
            Some(path) => PathBuf::from(path),
            None => directories::UserDirs::new()?.desktop_dir()?.to_owned(),
        };
        match Self::open(&root) {
            Ok(workspace) => Some(workspace),
            Err(error) => {
                tracing::warn!(workspace = %root.display(), %error, "workspace tools are unavailable");
                None
            }
        }
    }

    pub(super) fn open(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        #[cfg(windows)]
        let shell = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .map(|dir| dir.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        #[cfg(not(windows))]
        let shell = which::which("bash").ok();
        Ok(Self {
            root,
            dir,
            edits: Mutex::new(()),
            shell: shell.filter(|path| path.is_absolute() && path.is_file()),
        })
    }

    pub(crate) fn specs(&self) -> Vec<w::ClientCapabilitySpec> {
        let mut specs: Vec<_> = FILE_TOOLS.iter().map(|tool| (tool.spec)(self)).collect();
        if self.shell.is_some() {
            specs.push(self.spec::<ShellArgs>(
                SHELL,
                &format!("Run a {SHELL} command starting in the workspace and return its exit code, stdout and stderr. Time limit: {COMMAND_TIMEOUT} seconds."),
                "{command}",
            ));
        }
        specs
    }

    fn spec<T: JsonSchema>(
        &self,
        name: &str,
        description: &str,
        target: &str,
    ) -> w::ClientCapabilitySpec {
        let root = self.root.to_string_lossy();
        let mut schema = SchemaSettings::draft2020_12()
            .with(|settings| {
                settings.inline_subschemas = true;
                settings.meta_schema = None;
            })
            .into_generator()
            .into_root_schema_for::<Request<T>>()
            .to_value();
        schema["properties"]["workspace"]["const"] = json!(root);
        schema["additionalProperties"] = json!(false);
        w::ClientCapabilitySpec {
            name: format!("{PREFIX}{name}"),
            description: format!("{description} Workspace: {root}"),
            input_schema: schema.to_string(),
            approval: w::CapabilityApproval::Confirm.into(),
            approval_target: format!("{name} in {{workspace}}: {target}"),
            ..Default::default()
        }
    }

    pub(crate) async fn execute(
        self: Arc<Self>,
        capability: &str,
        input: String,
    ) -> Result<Value, WorkToolError> {
        if capability == WORKSPACE_SHELL_CAPABILITY {
            let ShellArgs { command } = self.arguments(&input)?;
            return Box::pin(self.shell(&command)).await;
        }
        let tool = FILE_TOOLS
            .iter()
            .find(|tool| tool.name == capability)
            .ok_or(WorkToolError::UnknownTool)?;
        tokio::task::spawn_blocking(move || (tool.run)(&self, &input)).await?
    }

    fn arguments<T: DeserializeOwned>(&self, input: &str) -> Result<T, WorkToolError> {
        let input = if input.trim().is_empty() { "{}" } else { input };
        let request: Request<T> = serde_json::from_str(input)?;
        if request.workspace != self.root.to_string_lossy() {
            return Err(WorkToolError::WorkspaceChanged);
        }
        Ok(request.tool)
    }

    fn grep(&self, arguments: GrepArgs) -> Result<Value, WorkToolError> {
        let GrepArgs {
            browse,
            pattern,
            ignore_case,
            literal,
            glob: filter,
        } = arguments;
        let pattern = if literal {
            regex::escape(&pattern)
        } else {
            pattern
        };
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .build()?;
        let filter = filter.as_deref().map(glob).transpose()?;
        self.search(
            &browse,
            Search::Grep {
                regex: &regex,
                glob: filter.as_ref(),
            },
        )
    }

    fn change(&self, path: &Path, change: FileChange) -> Result<Value, WorkToolError> {
        let _guard = self.edits.lock();
        let (content, create) = match change {
            FileChange::Write(content) => (content, true),
            FileChange::Edit(edits) => (edit(text(&self.load(path)?)?, &edits)?, false),
        };
        self.save(path, &content, create)?;
        Ok(json!({"path": path, "bytes": content.len()}))
    }

    pub(super) fn load(&self, path: &Path) -> Result<Vec<u8>, WorkToolError> {
        self.load_with_budget(path, &mut (FILE_LIMIT as u64 + 1))
    }

    pub(super) fn load_with_budget(
        &self,
        path: &Path,
        remaining: &mut u64,
    ) -> Result<Vec<u8>, WorkToolError> {
        // Check before opening so named pipes and device files are not read.
        let metadata = self.dir.metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::other("path must name a regular file").into());
        }
        if metadata.len() > FILE_LIMIT as u64 {
            return Err(WorkToolError::FileTooLarge);
        }
        if metadata.len() > *remaining {
            return Err(WorkToolError::SearchBudgetExhausted);
        }
        // A replacement FIFO between metadata and open must not block before fstat.
        let file = self
            .dir
            .open_with(path, OpenOptions::new().read(true).nonblock(true))?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("path must name a regular file").into());
        }
        let mut bytes = Vec::new();
        let limit = (FILE_LIMIT as u64 + 1).min(*remaining);
        let mut reader = file.take(limit);
        let result = reader.read_to_end(&mut bytes);
        *remaining -= limit - reader.limit();
        result?;
        if bytes.len() > FILE_LIMIT {
            return Err(WorkToolError::FileTooLarge);
        }
        // A file may have grown after the initial metadata check.
        if reader.limit() == 0 && reader.get_ref().metadata()?.len() > limit {
            return Err(WorkToolError::SearchBudgetExhausted);
        }
        Ok(bytes)
    }

    fn read(
        &self,
        path: &Path,
        offset: NonZeroUsize,
        limit: usize,
    ) -> Result<Value, WorkToolError> {
        let bytes = self.load(path)?;
        if is_attachment_image(&bytes) {
            if bytes.len() > IMAGE_LIMIT {
                return Err(WorkToolError::ImageTooLarge);
            }
            return Ok(json!({"path": path, "image_base64": STANDARD.encode(&bytes)}));
        }
        let offset = offset.get();
        let mut lines = text(&bytes)?
            .split_inclusive('\n')
            .skip(offset - 1)
            .peekable();
        let mut content = String::new();
        let mut next_line = offset;
        for _ in 0..limit {
            let Some(&line) = lines.peek() else { break };
            if content.len() + line.len() > OUTPUT_LIMIT {
                if content.is_empty() {
                    return Err(WorkToolError::LineTooLong);
                }
                break;
            }
            content.push_str(line);
            lines.next();
            next_line += 1;
        }
        Ok(json!({"path": path, "text": content, "next_offset": lines.peek().map(|_| next_line)}))
    }

    pub(super) fn save(
        &self,
        path: &Path,
        content: &str,
        create: bool,
    ) -> Result<(), WorkToolError> {
        if content.len() > FILE_LIMIT {
            return Err(WorkToolError::FileTooLarge);
        }
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("file name is required"))?;
        let folder = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if create {
            self.dir.create_dir_all(folder)?;
        }
        let folder = self.dir.open_dir(folder)?;
        let permissions = match folder.symlink_metadata(name) {
            Ok(meta) if meta.is_file() => Some(meta.permissions()),
            Ok(_) => return Err(io::Error::other("refusing to replace a directory or link").into()),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut temp = TempFile::new(&folder)?;
        let file = temp.as_file_mut();
        file.write_all(content.as_bytes())?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        Ok(temp.replace(name)?)
    }
}
