use super::{
    COMMAND_TIMEOUT, FILE_LIMIT, IMAGE_LIMIT, MAX_RESULTS, OUTPUT_LIMIT, PREFIX, SHELL,
    WorkToolError,
    helpers::{Edit, edit, glob, relative, text},
    search::{Browse, Search, max_results},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::{ambient_authority, fs::Dir};
use cap_tempfile::TempFile;
use ene_client_core::is_attachment_image;
use ene_rpc as w;
use parking_lot::Mutex;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
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

enum FileTool {
    Read(ReadArgs),
    Write(WriteArgs),
    Edit(EditArgs),
    Grep(GrepArgs),
    Find(FindArgs),
    Ls(Browse),
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

#[derive(Clone, Copy)]
enum FileChange<'a> {
    Write(&'a str),
    Edit(&'a [Edit]),
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
        let mut specs = vec![
            self.spec::<ReadArgs>("read", "Read text by line or a PNG, JPEG, GIF or WebP image.", "{path}"),
            self.spec::<WriteArgs>("write", "Create or replace a UTF-8 text file. Creates missing parent folders.", "{path}"),
            self.spec::<EditArgs>("edit", "Apply exact replacements in order. Each oldText must match exactly once. Save only if all replacements succeed.", "{path}"),
            self.spec::<GrepArgs>("grep", "Search UTF-8 files for matching lines. Pattern is a Rust regular expression. Path defaults to the workspace.", "{path}"),
            self.spec::<FindArgs>("find", "Find paths matching a glob, for example **/*.rs. Path defaults to the workspace.", "{path}"),
            self.spec::<Browse>("ls", "List a directory. Path defaults to the workspace.", "{path}"),
        ];
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
        input: Value,
    ) -> Result<Value, WorkToolError> {
        let tool = match capability.strip_prefix(PREFIX) {
            Some("read") => FileTool::Read(self.arguments(input)?),
            Some("write") => FileTool::Write(self.arguments(input)?),
            Some("edit") => FileTool::Edit(self.arguments(input)?),
            Some("grep") => FileTool::Grep(self.arguments(input)?),
            Some("find") => FileTool::Find(self.arguments(input)?),
            Some("ls") => FileTool::Ls(self.arguments(input)?),
            Some(SHELL) => {
                let ShellArgs { command } = self.arguments(input)?;
                return Box::pin(self.shell(&command)).await;
            }
            _ => return Err(WorkToolError::UnknownTool),
        };
        tokio::task::spawn_blocking(move || self.file_tool(tool)).await?
    }

    fn arguments<T: DeserializeOwned>(&self, input: Value) -> Result<T, WorkToolError> {
        let request: Request<T> = serde_json::from_value(input)?;
        if request.workspace != self.root.to_string_lossy() {
            return Err(WorkToolError::WorkspaceChanged);
        }
        Ok(request.tool)
    }

    fn file_tool(&self, tool: FileTool) -> Result<Value, WorkToolError> {
        match tool {
            FileTool::Read(ReadArgs {
                path,
                offset,
                limit,
            }) => self.read(relative(&path)?, offset, limit.get().min(MAX_RESULTS)),
            FileTool::Write(WriteArgs { path, content }) => {
                self.change(relative(&path)?, FileChange::Write(&content))
            }
            FileTool::Edit(EditArgs { path, edits }) => {
                self.change(relative(&path)?, FileChange::Edit(&edits))
            }
            FileTool::Ls(browse) => self.search(&browse, Search::List),
            FileTool::Find(FindArgs { browse, pattern }) => {
                self.search(&browse, Search::Find(&glob(&pattern)?))
            }
            FileTool::Grep(GrepArgs {
                browse,
                pattern,
                ignore_case,
                literal,
                glob: filter,
            }) => {
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
        }
    }

    fn change(&self, path: &Path, change: FileChange<'_>) -> Result<Value, WorkToolError> {
        let _guard = self.edits.lock();
        let (content, create) = match change {
            FileChange::Write(content) => (Cow::Borrowed(content), true),
            FileChange::Edit(edits) => (Cow::Owned(edit(text(&self.load(path)?)?, edits)?), false),
        };
        self.save(path, &content, create)?;
        Ok(json!({"path": path, "bytes": content.len()}))
    }

    pub(super) fn load(&self, path: &Path) -> io::Result<Vec<u8>> {
        // Check before opening so named pipes and device files are not read.
        let metadata = self.dir.metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::other("path must name a regular file"));
        }
        if metadata.len() > FILE_LIMIT as u64 {
            return Err(io::Error::other("file exceeds the 4 MiB limit"));
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let mut file = {
            use cap_std::fs::{OpenOptions, OpenOptionsExt as _};
            // A replacement FIFO between metadata and open must not block before fstat.
            self.dir.open_with(
                path,
                OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK),
            )?
        };
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let mut file = self.dir.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("path must name a regular file"));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > FILE_LIMIT {
            return Err(io::Error::other("file exceeds the 4 MiB limit"));
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
            let Some(line) = lines.next() else { break };
            if content.len() + line.len() > OUTPUT_LIMIT {
                if content.is_empty() {
                    return Err(WorkToolError::LineTooLong);
                }
                return Ok(json!({"path": path, "text": content, "next_offset": next_line}));
            }
            content.push_str(line);
            next_line += 1;
        }
        Ok(json!({"path": path, "text": content, "next_offset": lines.peek().map(|_| next_line)}))
    }

    pub(super) fn save(&self, path: &Path, content: &str, create: bool) -> io::Result<()> {
        if content.len() > FILE_LIMIT {
            return Err(io::Error::other("file exceeds the 4 MiB limit"));
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
            Ok(_) => return Err(io::Error::other("refusing to replace a directory or link")),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut temp = TempFile::new(&folder)?;
        let file = temp.as_file_mut();
        file.write_all(content.as_bytes())?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        temp.replace(name)
    }
}
