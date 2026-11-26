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

use serde::Deserialize;
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

#[derive(Deserialize)]
struct Request {
    workspace: String,
    #[serde(flatten)]
    tool: Tool,
}

#[derive(Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
enum Tool {
    Read {
        path: String,
        #[serde(default = "first_line")]
        offset: NonZeroUsize,
        #[serde(default = "max_results")]
        limit: NonZeroUsize,
    },
    Write {
        path: String,
        content: String,
    },
    Edit {
        path: String,
        edits: Vec<Edit>,
    },
    Ls {
        #[serde(flatten)]
        browse: Browse,
    },
    Find {
        #[serde(flatten)]
        browse: Browse,
        pattern: String,
    },
    Grep {
        #[serde(flatten)]
        browse: Browse,
        pattern: String,
        #[serde(default, rename = "ignoreCase")]
        ignore_case: bool,
        #[serde(default)]
        literal: bool,
        glob: Option<String>,
    },
    #[cfg_attr(windows, serde(rename = "powershell"))]
    #[cfg_attr(not(windows), serde(rename = "bash"))]
    Shell {
        command: String,
    },
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
        let path = json!({"type": "string", "description": "Path relative to the workspace, using / between components."});
        let limit = json!({"type": "integer", "minimum": 1, "maximum": MAX_RESULTS});
        let mut tools = vec![
            self.spec("read", "Read text by line or a PNG, JPEG, GIF or WebP image.",
                json!({"path": path, "offset": {"type": "integer", "minimum": 1}, "limit": limit}),
                &["path"], "{path}"),
            self.spec("write", "Create or replace a UTF-8 text file. Creates missing parent folders.",
                json!({"path": path, "content": {"type": "string"}}), &["path", "content"], "{path}"),
            self.spec("edit", "Apply exact replacements in order. Each oldText must match exactly once. Save only if all replacements succeed.",
                json!({"path": path, "edits": {"type": "array", "minItems": 1, "items": {
                    "type": "object", "properties": {"oldText": {"type": "string", "minLength": 1}, "newText": {"type": "string"}},
                    "required": ["oldText", "newText"], "additionalProperties": false
                }}}), &["path", "edits"], "{path}"),
            self.spec("grep", "Search UTF-8 files for matching lines. Pattern is a Rust regular expression. Path defaults to the workspace.",
                json!({"path": path, "pattern": {"type": "string"}, "ignoreCase": {"type": "boolean"},
                    "literal": {"type": "boolean"}, "glob": {"type": "string"}, "limit": limit}),
                &["pattern"], "{path}"),
            self.spec("find", "Find paths matching a glob, for example **/*.rs. Path defaults to the workspace.",
                json!({"path": path, "pattern": {"type": "string"}, "limit": limit}), &["pattern"], "{path}"),
            self.spec("ls", "List a directory. Path defaults to the workspace.",
                json!({"path": path, "limit": limit}), &[], "{path}"),
        ];
        if self.shell.is_some() {
            tools.push(self.spec(
                SHELL,
                &format!("Run a {SHELL} command starting in the workspace and return its exit code, stdout and stderr. Time limit: {COMMAND_TIMEOUT} seconds."),
                json!({"command": {"type": "string", "minLength": 1}}),
                &["command"],
                "{command}",
            ));
        }
        tools
    }

    pub(super) fn spec(
        &self,
        name: &str,
        description: &str,
        mut properties: Value,
        required: &[&str],
        target: &str,
    ) -> w::ClientCapabilitySpec {
        properties["workspace"] = json!({
            "type": "string", "const": self.root.to_string_lossy(),
            "description": "Copy this workspace path exactly."
        });
        let mut required = required.to_vec();
        required.push("workspace");
        w::ClientCapabilitySpec {
            name: format!("{PREFIX}{name}"),
            description: format!("{description} Workspace: {}", self.root.display()),
            input_schema: json!({
                "type": "object", "properties": properties, "required": required, "additionalProperties": false
            }).to_string(),
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
        let tool = capability
            .strip_prefix(PREFIX)
            .ok_or(WorkToolError::UnknownTool)?;
        let Value::Object(mut fields) = input else {
            return Err(WorkToolError::InvalidArguments);
        };
        fields.insert("tool".into(), tool.into());
        let request: Request = serde_json::from_value(Value::Object(fields))?;
        if request.workspace != self.root.to_string_lossy() {
            return Err(WorkToolError::WorkspaceChanged);
        }
        match request.tool {
            Tool::Shell { command } => Box::pin(self.shell(&command)).await,
            tool => tokio::task::spawn_blocking(move || self.file_tool(tool)).await?,
        }
    }

    fn file_tool(&self, tool: Tool) -> Result<Value, WorkToolError> {
        match tool {
            Tool::Read {
                path,
                offset,
                limit,
            } => self.read(relative(&path)?, offset, limit.get().min(MAX_RESULTS)),
            Tool::Write { path, content } => self.change(relative(&path)?, Some(&content), &[]),
            Tool::Edit { path, edits } => self.change(relative(&path)?, None, &edits),
            Tool::Ls { browse } => self.search(&browse, Search::List),
            Tool::Find { browse, pattern } => self.search(&browse, Search::Find(&glob(&pattern)?)),
            Tool::Grep {
                browse,
                pattern,
                ignore_case,
                literal,
                glob: filter,
            } => {
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
            Tool::Shell { .. } => Err(WorkToolError::UnknownTool),
        }
    }

    fn change(
        &self,
        path: &Path,
        content: Option<&str>,
        edits: &[Edit],
    ) -> Result<Value, WorkToolError> {
        let _guard = self.edits.lock();
        let len = match content {
            Some(content) => {
                self.save(path, content, true)?;
                content.len()
            }
            None => {
                let content = edit(text(&self.load(path)?)?, edits)?;
                self.save(path, &content, false)?;
                content.len()
            }
        };
        Ok(json!({"path": path, "bytes": len}))
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
