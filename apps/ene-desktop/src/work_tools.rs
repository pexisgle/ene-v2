//! Workspace operations executed by the desktop client for a work session.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use ene_rpc as w;
use globset::{GlobBuilder, GlobMatcher};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};

const FILE_LIMIT: usize = 4 * 1024 * 1024;
const IMAGE_LIMIT: usize = 512 * 1024;
const OUTPUT_LIMIT: usize = 24 * 1024;
const SCAN_LIMIT: usize = 20_000;
const GREP_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
const COMMAND_TIMEOUT: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkToolError {
    #[error("the workspace has changed")]
    WorkspaceChanged,
    #[error("unknown workspace tool")]
    UnknownTool,
    #[error("file writer is unavailable")]
    WriterUnavailable,
    #[error("path must be a string")]
    PathNotString,
    #[error("path is required")]
    MissingPath,
    #[error("path must stay relative to the workspace")]
    InvalidPath,
    #[error("{0} must be a string")]
    NotString(&'static str),
    #[error("{0} must be a positive integer")]
    NotPositiveInt(&'static str),
    #[error("image exceeds the 512 KiB limit")]
    ImageTooLarge,
    #[error("line exceeds the output limit. Use grep or a shell command to inspect part of it.")]
    LineTooLong,
    #[error("file is binary")]
    BinaryFile,
    #[error("file is not UTF-8")]
    NotUtf8,
    #[error("edits must be a nonempty array")]
    EmptyEdits,
    #[error("oldText must not be empty")]
    EmptyOldText,
    #[error("oldText did not match the file")]
    OldTextNotFound,
    #[error("oldText matched more than once")]
    OldTextNotUnique,
    #[error("file exceeds the 4 MiB limit")]
    FileTooLarge,
    #[error("the requested shell is unavailable")]
    ShellUnavailable,
    #[error("command is empty")]
    EmptyCommand,
    #[error("the shell process is unavailable")]
    ShellProcessUnavailable,
    #[error("the shell process pipes are unavailable")]
    ShellPipesUnavailable,
    #[error("command exceeded {COMMAND_TIMEOUT} seconds")]
    CommandTimedOut,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    Glob(#[from] globset::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

pub(crate) const PREFIX: &str = "work.";
pub const WORKSPACE_FILE_CAPABILITIES: &[&str] = &[
    "work.read",
    "work.write",
    "work.edit",
    "work.grep",
    "work.find",
    "work.ls",
];

#[cfg(windows)]
const SHELL: &str = "powershell";
#[cfg(not(windows))]
const SHELL: &str = "bash";

#[cfg(windows)]
pub(crate) const WORKSPACE_SHELL_CAPABILITY: &str = "work.powershell";
#[cfg(not(windows))]
pub(crate) const WORKSPACE_SHELL_CAPABILITY: &str = "work.bash";

pub(crate) struct Workspace {
    root: PathBuf,
    dir: Dir,
    edits: Mutex<()>,
    shell: Option<PathBuf>,
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

    fn open(root: &Path) -> io::Result<Self> {
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

    fn spec(
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

    fn file_tool(&self, name: &str, input: &Value) -> Result<Value, WorkToolError> {
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

    fn load(&self, path: &Path) -> io::Result<Vec<u8>> {
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

    fn read(&self, path: &Path, offset: usize, limit: usize) -> Result<Value, WorkToolError> {
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

    fn save(&self, path: &Path, content: &str, create: bool) -> io::Result<()> {
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

    fn search(
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

    fn walk(
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

    async fn shell(&self, command: &str) -> Result<Value, WorkToolError> {
        let program = self.shell.as_ref().ok_or(WorkToolError::ShellUnavailable)?;
        if command.trim().is_empty() {
            return Err(WorkToolError::EmptyCommand);
        }
        let mut process = CommandWrap::with_new(program, |child| {
            child
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(not(windows))]
            child.args(["--noprofile", "--norc", "-c", command]);
            #[cfg(windows)]
            {
                child.creation_flags(0x08000000); // CREATE_NO_WINDOW
                let script =
                    format!("[Console]::OutputEncoding = [Text.UTF8Encoding]::new(); {command}");
                let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
                child
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-EncodedCommand",
                    ])
                    .arg(STANDARD.encode(bytes));
            }
        });
        #[cfg(unix)]
        process.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        process.wrap(process_wrap::tokio::JobObject);
        process.wrap(KillOnDrop);
        let mut running = RunningShell(Some(process.spawn()?));
        let Some(child) = running.0.as_mut() else {
            return Err(WorkToolError::ShellProcessUnavailable);
        };
        let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
            return Err(WorkToolError::ShellPipesUnavailable);
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(COMMAND_TIMEOUT),
            Box::pin(async { tokio::try_join!(child.wait(), capture(stdout), capture(stderr)) }),
        )
        .await;
        let (status, (stdout, stdout_cut), (stderr, stderr_cut)) = match outcome {
            Ok(result) => result?,
            Err(_) => return Err(WorkToolError::CommandTimedOut),
        };
        running.0 = None;
        Ok(json!({
            "success": status.success(), "exit_code": status.code(), "stdout": stdout, "stderr": stderr,
            "truncated": stdout_cut || stderr_cut
        }))
    }
}

struct RunningShell(Option<Box<dyn ChildWrapper>>);

impl Drop for RunningShell {
    fn drop(&mut self) {
        // Kill the group or job even if the device stream drops the future.
        if let Some(child) = &mut self.0 {
            ene_platform_os::warn_if_err(child.start_kill(), "tool process termination");
        }
    }
}

fn relative(path: &str) -> Result<&Path, WorkToolError> {
    let parsed = Path::new(path);
    if path.is_empty()
        || (cfg!(windows) && path.contains(':'))
        || parsed
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(WorkToolError::InvalidPath);
    }
    Ok(parsed)
}

fn required<'a>(input: &'a Value, name: &'static str) -> Result<&'a str, WorkToolError> {
    input[name].as_str().ok_or(WorkToolError::NotString(name))
}

fn positive(input: &Value, name: &'static str, default: usize) -> Result<usize, WorkToolError> {
    match input.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| value > 0)
            .ok_or(WorkToolError::NotPositiveInt(name)),
    }
}

fn text(bytes: &[u8]) -> Result<&str, WorkToolError> {
    if bytes.contains(&0) {
        return Err(WorkToolError::BinaryFile);
    }
    std::str::from_utf8(bytes).map_err(|_| WorkToolError::NotUtf8)
}

fn image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

fn edit(content: &str, input: &Value) -> Result<String, WorkToolError> {
    let edits = input["edits"]
        .as_array()
        .filter(|edits| !edits.is_empty())
        .ok_or(WorkToolError::EmptyEdits)?;
    let mut content = content.to_owned();
    for edit in edits {
        let old = required(edit, "oldText")?;
        let new = required(edit, "newText")?;
        if old.is_empty() {
            return Err(WorkToolError::EmptyOldText);
        }
        let Some(index) = content.find(old) else {
            return Err(WorkToolError::OldTextNotFound);
        };
        let end = index + old.len();
        // Check overlapping occurrences as well.
        let Some(first_char) = old.chars().next() else {
            return Err(WorkToolError::EmptyOldText);
        };
        let next = index + first_char.len_utf8();
        if content[next..].contains(old) {
            return Err(WorkToolError::OldTextNotUnique);
        }
        if content.len() - old.len() + new.len() > FILE_LIMIT {
            return Err(WorkToolError::FileTooLarge);
        }
        content.replace_range(index..end, new);
    }
    Ok(content)
}

fn glob(pattern: &str) -> Result<(GlobMatcher, bool), WorkToolError> {
    Ok((
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()?
            .compile_matcher(),
        pattern.contains('/'),
    ))
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> io::Result<(String, bool)> {
    let mut buffer = [0; 8192];
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT - output.len());
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_tools_read_search_and_edit_only_the_bound_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::open(directory.path()).unwrap());
        let call = |name: &str, mut input: Value| {
            input["workspace"] = json!(workspace.root.to_string_lossy());
            let workspace = workspace.clone();
            let name = format!("work.{name}");
            async move { Box::pin(workspace.execute(&name, input)).await }
        };

        call(
            "write",
            json!({"path": "src/index.html", "content": "header\nhello\nfooter\n"}),
        )
        .await
        .unwrap();
        let read = call(
            "read",
            json!({"path": "src/index.html", "offset": 2, "limit": 1}),
        )
        .await
        .unwrap();
        assert_eq!(read["text"], "hello\n");
        assert_eq!(read["next_offset"], 3);
        let grep = call(
            "grep",
            json!({"pattern": "^HELLO$", "ignoreCase": true, "glob": "**/*.html"}),
        )
        .await
        .unwrap();
        assert_eq!(grep["entries"][0]["line"], 2);
        let find = call("find", json!({"pattern": "**/*.html"})).await.unwrap();
        assert_eq!(find["entries"].as_array().unwrap().len(), 1);
        let ls = call("ls", json!({})).await.unwrap();
        assert_eq!(ls["entries"][0], json!({"path": "src", "directory": true}));

        assert!(
            call(
                "edit",
                json!({"path": "src/index.html", "edits": [
                    {"oldText": "hello", "newText": "world"},
                    {"oldText": "missing", "newText": "oops"}
                ]})
            )
            .await
            .is_err()
        );
        let file = directory.path().join("src/index.html");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "header\nhello\nfooter\n"
        );
        call(
            "edit",
            json!({"path": "src/index.html", "edits": [{"oldText": "hello", "newText": "world"}]}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "header\nworld\nfooter\n"
        );
        assert!(
            call("write", json!({"path": "../escape.html", "content": "bad"}))
                .await
                .is_err()
        );
        assert!(
            workspace
                .clone()
                .execute(
                    "work.write",
                    json!({
                        "workspace": "other", "path": "src/index.html", "content": "bad"
                    })
                )
                .await
                .is_err()
        );

        if workspace.shell.is_some() {
            #[cfg(windows)]
            let command = "Get-Content -Raw src/index.html";
            #[cfg(not(windows))]
            let command = "cat src/index.html";
            let result = call(SHELL, json!({"command": command})).await.unwrap();
            assert_eq!(result["exit_code"], 0, "{result}");
            assert!(result["stdout"].as_str().unwrap().contains("world"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_operations_cannot_follow_a_link_out_of_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        assert!(workspace.load(Path::new("link/secret")).is_err());
        assert!(workspace.save(Path::new("link/new"), "bad", true).is_err());
        assert!(!outside.path().join("new").exists());
    }
}
