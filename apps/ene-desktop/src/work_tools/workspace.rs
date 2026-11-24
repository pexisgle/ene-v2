use super::{
    COMMAND_TIMEOUT, FILE_LIMIT, GREP_BYTES, IMAGE_LIMIT, MAX_RESULTS, OUTPUT_LIMIT, PREFIX,
    SCAN_LIMIT, SHELL, WorkToolError,
    helpers::{edit, glob, image, positive, relative, required, text},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use ene_rpc as w;

use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) struct Workspace {
    pub(super) root: PathBuf,
    dir: Dir,
    edits: Mutex<()>,
    pub(super) shell: Option<PathBuf>,
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
        if required(&input, "workspace")? != self.root.to_string_lossy() {
            return Err(WorkToolError::WorkspaceChanged);
        }
        let name = capability
            .strip_prefix(PREFIX)
            .ok_or(WorkToolError::UnknownTool)?;
        if name == SHELL {
            return Box::pin(self.shell(required(&input, "command")?)).await;
        }
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || self.file_tool(&name, &input)).await?
    }

    pub(super) fn file_tool(&self, name: &str, input: &Value) -> Result<Value, WorkToolError> {
        let path = match input.get("path") {
            Some(path) => path.as_str().ok_or(WorkToolError::PathNotString)?,
            None if matches!(name, "ls" | "grep" | "find") => ".",
            None => return Err(WorkToolError::MissingPath),
        };
        let path = relative(path)?;
        let limit = positive(input, "limit", MAX_RESULTS)?.min(MAX_RESULTS);
        match name {
            "read" => self.read(path, positive(input, "offset", 1)?, limit),
            "write" | "edit" => {
                let _guard = self
                    .edits
                    .lock()
                    .map_err(|_| WorkToolError::WriterUnavailable)?;
                let content = if name == "edit" {
                    let bytes = self.load(path)?;
                    edit(text(&bytes)?, input)?
                } else {
                    required(input, "content")?.to_owned()
                };
                self.save(path, &content, name == "write")?;
                Ok(json!({"path": path, "bytes": content.len()}))
            }
            "ls" | "find" | "grep" => self.search(name, path, input, limit),
            _ => Err(WorkToolError::UnknownTool),
        }
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

    pub(super) fn read(
        &self,
        path: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<Value, WorkToolError> {
        let bytes = self.load(path)?;
        if image(&bytes) {
            if bytes.len() > IMAGE_LIMIT {
                return Err(WorkToolError::ImageTooLarge);
            }
            return Ok(json!({"path": path, "image_base64": STANDARD.encode(&bytes)}));
        }
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
        let temp = format!(".ene-{}.tmp", uuid::Uuid::new_v4());
        let mut file = folder.open_with(&temp, OpenOptions::new().write(true).create_new(true))?;
        let saved = (|| {
            file.write_all(content.as_bytes())?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()
        })();
        drop(file);
        let saved = saved.and_then(|()| folder.rename(&temp, &folder, name));
        if saved.is_err() {
            ene_platform_os::debug_if_err(folder.remove_file(&temp), "temporary file cleanup");
        }
        saved
    }

    pub(super) fn search(
        &self,
        name: &str,
        path: &Path,
        input: &Value,
        limit: usize,
    ) -> Result<Value, WorkToolError> {
        let glob = match name {
            "find" => Some(glob(required(input, "pattern")?)?),
            "grep" => input
                .get("glob")
                .map(|_| required(input, "glob").and_then(glob))
                .transpose()?,
            _ => None,
        };
        let regex = if name == "grep" {
            let pattern = required(input, "pattern")?;
            let pattern = if input["literal"].as_bool() == Some(true) {
                regex::escape(pattern)
            } else {
                pattern.to_owned()
            };
            Some(
                regex::RegexBuilder::new(&pattern)
                    .case_insensitive(input["ignoreCase"].as_bool() == Some(true))
                    .build()?,
            )
        } else {
            None
        };

        let mut entries = Vec::new();
        let mut remaining_bytes = GREP_BYTES;
        let mut output_bytes = 0;
        let mut scanned = 0;
        let mut truncated = false;
        let mut skipped = 0;
        let start = path;
        let mut visit = |path: &Path, is_file: bool, is_dir: bool| -> bool {
            scanned += 1;
            if scanned > SCAN_LIMIT {
                truncated = true;
                return false;
            }
            if glob.as_ref().is_some_and(|(glob, nested)| {
                let candidate = if *nested {
                    path.strip_prefix(start).unwrap_or(path)
                } else {
                    Path::new(path.file_name().unwrap_or_default())
                };
                !glob.is_match(candidate)
            }) {
                return true;
            }
            if let Some(regex) = &regex {
                if !is_file {
                    return true;
                }
                let Ok(bytes) = self.load(path) else {
                    skipped += 1;
                    return true;
                };
                if bytes.len() > remaining_bytes {
                    truncated = true;
                    return false;
                }
                remaining_bytes -= bytes.len();
                let Ok(text) = text(&bytes) else { return true };
                for (index, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        let prefix = &line[..line.floor_char_boundary(2048)];
                        truncated |= prefix.len() < line.len();
                        let line = prefix;
                        output_bytes += path.as_os_str().len() + line.len();
                        if entries.len() >= limit || output_bytes > OUTPUT_LIMIT {
                            truncated = true;
                            return false;
                        }
                        entries.push(json!({"path": path.to_string_lossy(), "line": index + 1, "text": line}));
                    }
                }
            } else {
                output_bytes += path.as_os_str().len();
                if entries.len() >= limit || output_bytes > OUTPUT_LIMIT {
                    truncated = true;
                    return false;
                }
                entries.push(json!({"path": path.to_string_lossy(), "directory": is_dir}));
            }
            true
        };
        if name == "grep"
            && self
                .dir
                .symlink_metadata(path)
                .is_ok_and(|meta| meta.is_file())
        {
            visit(path, true, false);
        } else {
            self.walk(path, name != "ls", &mut visit)?;
        }
        entries.sort_by(|left, right| {
            left["path"]
                .as_str()
                .cmp(&right["path"].as_str())
                .then_with(|| left["line"].as_u64().cmp(&right["line"].as_u64()))
        });
        Ok(json!({"entries": entries, "truncated": truncated, "skipped_files": skipped}))
    }

    pub(super) fn walk(
        &self,
        root: &Path,
        recursive: bool,
        visit: &mut impl FnMut(&Path, bool, bool) -> bool,
    ) -> io::Result<()> {
        // A stack of directory iterators bounds open handles by depth and
        // never follows a symlink encountered in the listing.
        let mut stack = vec![(root.to_owned(), self.dir.read_dir(root)?)];
        while let Some((parent, entries)) = stack.last_mut() {
            let Some(entry) = entries.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            let name = entry.file_name();
            let path = if parent == Path::new(".") {
                PathBuf::from(&name)
            } else {
                parent.join(&name)
            };
            let kind = entry.file_type()?;
            if !visit(&path, kind.is_file(), kind.is_dir()) {
                break;
            }
            if recursive && kind.is_dir() && name != ".git" && stack.len() < 64 {
                match self.dir.read_dir(&path) {
                    Ok(entries) => stack.push((path, entries)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}
