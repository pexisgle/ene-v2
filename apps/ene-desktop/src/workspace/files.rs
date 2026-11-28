use super::{
    FILE_LIMIT, IMAGE_LIMIT, MAX_RESULTS, OUTPUT_LIMIT, WorkToolError,
    helpers::{Edit, edit, glob, relative, text},
    search::{Browse, Search, max_results},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::{ambient_authority, fs::Dir};
use cap_tempfile::TempFile;
use ene_rpc as w;
use parking_lot::Mutex;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    borrow::Cow,
    io::{self, Read, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Workspace {
    root: PathBuf,
    binding: String,
    pub(super) dir: Dir,
    edits: Mutex<()>,
}

#[derive(Deserialize, JsonSchema)]
struct Request<T> {
    /// Copy the selected workspace identifier exactly.
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
    /// Content revision from read. Omit to create a new file.
    revision: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// Path relative to the workspace, using / between components.
    path: String,
    #[schemars(length(min = 1))]
    edits: Vec<Edit>,
    /// Content revision from read.
    revision: String,
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

enum FileChange<'a> {
    Write(&'a str, Option<&'a str>),
    Edit(&'a [Edit], &'a str),
}

struct FileTool {
    name: &'static str,
    spec: fn(&Workspace) -> w::Capability,
    run: fn(&Workspace, Value) -> Result<Value, WorkToolError>,
}

// Declare each tool's advertised schema and parser together.
macro_rules! file_tools {
    ($prefix:literal; $($name:literal: $args:ty, $description:literal, $run:expr;)+) => {
        pub(crate) const PREFIX: &str = $prefix;
        const FILE_TOOLS: &[FileTool] = &[$(FileTool {
            name: concat!($prefix, $name),
            spec: |workspace| workspace.spec::<$args>($name, $description),
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
        |workspace: &Workspace, WriteArgs { path, content, revision }| {
            workspace.change(relative(&path)?, FileChange::Write(&content, revision.as_deref()))
        };
    "edit": EditArgs, "Apply exact replacements in order. Each oldText must match exactly once. Save only if all replacements succeed.",
        |workspace: &Workspace, EditArgs { path, edits, revision }| {
            workspace.change(relative(&path)?, FileChange::Edit(&edits, &revision))
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
    pub(crate) fn probe(configured: Option<&Path>) -> Option<Self> {
        let root = match configured {
            Some(path) => path.to_owned(),
            None => match std::env::var_os("ENE_DESKTOP_WORKSPACE") {
                Some(path) => PathBuf::from(path),
                None => directories::UserDirs::new()?.desktop_dir()?.to_owned(),
            },
        };
        Self::open(&root).ok()
    }

    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        if root.as_os_str().is_empty() {
            return Err(io::Error::other("workspace path is empty"));
        }
        let root = root.canonicalize()?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())?;
        let binding = hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()));
        Ok(Self {
            root,
            binding,
            dir,
            edits: Mutex::new(()),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn specs(&self) -> Vec<w::Capability> {
        FILE_TOOLS.iter().map(|tool| (tool.spec)(self)).collect()
    }

    fn spec<T: JsonSchema>(&self, name: &str, description: &str) -> w::Capability {
        let mut schema = SchemaSettings::draft2020_12()
            .with(|settings| {
                settings.inline_subschemas = true;
                settings.meta_schema = None;
            })
            .into_generator()
            .into_root_schema_for::<Request<T>>()
            .to_value();
        schema["properties"]["workspace"]["const"] = json!(self.binding);
        schema["additionalProperties"] = json!(false);
        w::Capability {
            name: format!("{PREFIX}{name}"),
            schema_json: schema.to_string(),
            description: description.into(),
            ..Default::default()
        }
    }

    pub(crate) async fn execute(
        self: Arc<Self>,
        capability: &str,
        input: Value,
    ) -> Result<Value, WorkToolError> {
        let tool = FILE_TOOLS
            .iter()
            .find(|tool| tool.name == capability)
            .ok_or(WorkToolError::UnknownTool)?;
        tokio::task::spawn_blocking(move || (tool.run)(&self, input)).await?
    }

    fn arguments<T: DeserializeOwned>(&self, input: Value) -> Result<T, WorkToolError> {
        let request: Request<T> = serde_json::from_value(input)?;
        if request.workspace != self.binding {
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

    fn change(&self, path: &Path, change: FileChange<'_>) -> Result<Value, WorkToolError> {
        let _guard = self.edits.lock();
        let (content, create) = match change {
            FileChange::Write(content, None) => (Cow::Borrowed(content), true),
            FileChange::Write(content, Some(revision)) => {
                self.load_revision(path, revision)?;
                (Cow::Borrowed(content), false)
            }
            FileChange::Edit(edits, revision) => {
                let previous = self.load_revision(path, revision)?;
                (Cow::Owned(edit(text(&previous)?, edits)?), false)
            }
        };
        self.save(path, &content, create)?;
        Ok(json!({"path": path, "bytes": content.len()}))
    }

    fn load_revision(&self, path: &Path, revision: &str) -> Result<Vec<u8>, WorkToolError> {
        let bytes = self.load(path)?;
        if revision != hex::encode(Sha256::digest(&bytes)) {
            return Err(WorkToolError::StaleFile);
        }
        Ok(bytes)
    }

    pub(super) fn load(&self, path: &Path) -> Result<Vec<u8>, WorkToolError> {
        // Check before opening so named pipes and device files are not read.
        let metadata = self.dir.metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::other("path must name a regular file").into());
        }
        if metadata.len() > FILE_LIMIT as u64 {
            return Err(WorkToolError::FileTooLarge);
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let file = {
            use cap_std::fs::{OpenOptions, OpenOptionsExt as _};
            // A replacement FIFO between metadata and open must not block before fstat.
            self.dir.open_with(
                path,
                OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK),
            )?
        };
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let file = self.dir.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("path must name a regular file").into());
        }
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > FILE_LIMIT {
            return Err(WorkToolError::FileTooLarge);
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
        if image::guess_format(&bytes).is_ok() {
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
        Ok(
            json!({"path": path, "text": content, "revision": hex::encode(Sha256::digest(&bytes)), "next_offset": lines.peek().map(|_| next_line)}),
        )
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
            Ok(meta) if meta.is_file() && !create => Some(meta.permissions()),
            Ok(_) => return Err(io::Error::other("refusing to replace a directory or link").into()),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if create {
            let mut created = folder.open_with(
                name,
                cap_std::fs::OpenOptions::new().write(true).create_new(true),
            )?;
            if let Err(error) = created
                .write_all(content.as_bytes())
                .and_then(|()| created.sync_all())
            {
                drop(created);
                let _removed = folder.remove_file(name);
                return Err(error.into());
            }
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_write_requires_the_revision_that_was_read() -> Result<(), WorkToolError> {
        let directory = tempfile::tempdir()?;
        let workspace = Workspace::open(directory.path())?;
        let path = Path::new("notes.md");
        workspace.change(path, FileChange::Write("before", None))?;
        let result = workspace.read(path, NonZeroUsize::MIN, 1)?;
        let revision = result["revision"].as_str().expect("file revision");
        std::fs::write(directory.path().join(path), "changed by user")?;
        assert!(matches!(
            workspace.change(path, FileChange::Write("overwrite", Some(revision))),
            Err(WorkToolError::StaleFile)
        ));
        assert!(
            workspace
                .change(path, FileChange::Write("create", None))
                .is_err()
        );
        assert_eq!(workspace.load(path)?, b"changed by user");
        Ok(())
    }
}
