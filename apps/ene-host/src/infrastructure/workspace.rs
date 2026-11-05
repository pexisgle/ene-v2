//! Workspace-scoped text file access for host-mediated tools.
//!
//! Every access is resolved through a capability on the canonical workspace
//! root: one component at a time, with symlinks refused on every step, so a
//! replaced path element cannot redirect a checked path to another file.
//! Writes are atomic and never create directories implicitly.
use crate::domain::{Error, ErrorKind};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const MAX_PATH: usize = 4096;
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_WRITE_BYTES: usize = 256 * 1024;
const MAX_LINES: u32 = 10_000;

#[derive(Clone, Debug)]
pub(crate) struct Workspace {
    root: PathBuf,
    dir: Arc<Dir>,
}

pub(crate) struct ReadText {
    pub content: String,
    pub truncated: bool,
}

impl Workspace {
    pub(crate) fn open(root: &str) -> Result<Self, Error> {
        if root.is_empty()
            || root.len() > MAX_PATH
            || root.chars().any(char::is_control)
            || !Path::new(root).is_absolute()
        {
            return Err(Error::invalid());
        }
        let root = std::fs::canonicalize(root).map_err(|_| Error::new(ErrorKind::Unavailable))?;
        if !std::fs::metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(Error::new(ErrorKind::Unavailable));
        }
        let dir = Dir::open_ambient_dir(&root, ambient_authority()).map_err(io_failure)?;
        Ok(Self {
            root,
            dir: Arc::new(dir),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Directory inside the workspace whose every component is a real
    /// directory; symlinks and escapes are rejected. The returned path is the
    /// verified one, for callers that need a process-level path.
    pub(crate) fn directory(&self, path: &str) -> Result<PathBuf, Error> {
        let relative = self.beneath(path)?;
        let mut dir = self.dir.try_clone().map_err(io_failure)?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(denied());
            };
            dir = dir.open_dir_nofollow(name).map_err(|_| denied())?;
        }
        Ok(self.root.join(relative))
    }

    pub(crate) fn read(
        &self,
        path: &str,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<ReadText, Error> {
        let relative = relative(path)?;
        if line == Some(0) || limit == Some(0) || limit.is_some_and(|limit| limit > MAX_LINES) {
            return Err(Error::invalid());
        }
        let (parent, name) = self.parent(&relative)?;
        let metadata = parent
            .symlink_metadata(name)
            .map_err(|_| Error::new(ErrorKind::NotFound))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(denied());
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = parent.open_with(name, &options).map_err(io_failure)?;
        // Read one byte past the limit so an oversized file is refused without
        // trusting its current size.
        let mut bytes = Vec::new();
        file.take((MAX_READ_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_failure)?;
        if bytes.len() > MAX_READ_BYTES {
            return Err(Error::new(ErrorKind::ResourceLimit));
        }
        let content =
            String::from_utf8(bytes).map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let start = line.unwrap_or(1).saturating_sub(1) as usize;
        if start >= lines.len() {
            return Ok(ReadText {
                content: String::new(),
                truncated: false,
            });
        }
        let end = limit
            .map(|limit| start.saturating_add(limit as usize))
            .unwrap_or(lines.len())
            .min(lines.len());
        Ok(ReadText {
            content: lines[start..end].concat(),
            truncated: end < lines.len(),
        })
    }

    pub(crate) fn write(&self, path: &str, content: &str) -> Result<(), Error> {
        if content.len() > MAX_WRITE_BYTES {
            return Err(Error::new(ErrorKind::ResourceLimit));
        }
        let relative = relative(path)?;
        let (parent, name) = self.parent(&relative)?;
        if let Ok(metadata) = parent.symlink_metadata(name)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(denied());
        }
        // The temporary file and the rename share one directory handle, so the
        // commit lands in the checked directory even if the path is replaced.
        let temporary = format!(".ene-{}.tmp", uuid::Uuid::new_v4());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let result = (|| {
            let mut file = parent.open_with(&temporary, &options).map_err(io_failure)?;
            file.write_all(content.as_bytes()).map_err(io_failure)?;
            file.sync_all().map_err(io_failure)?;
            parent.rename(&temporary, &parent, name).map_err(io_failure)
        })();
        let _ = parent.remove_file_or_symlink(&temporary);
        result
    }

    /// The workspace-relative form of a path that is either relative or
    /// absolute beneath the root.
    fn beneath(&self, path: &str) -> Result<PathBuf, Error> {
        if path.len() > MAX_PATH || path.chars().any(char::is_control) {
            return Err(denied());
        }
        let path = Path::new(path);
        let relative = match path.strip_prefix(&self.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) if path.is_absolute() => return Err(denied()),
            Err(_) => path.to_path_buf(),
        };
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(denied());
        }
        Ok(relative)
    }

    /// The verified parent directory and the final file name, so the open that
    /// follows uses the object that was checked.
    fn parent<'a>(&self, relative: &'a Path) -> Result<(Dir, &'a std::ffi::OsStr), Error> {
        let name = relative.file_name().ok_or_else(denied)?;
        let mut dir = self.dir.try_clone().map_err(io_failure)?;
        for component in relative.parent().unwrap_or(Path::new("")).components() {
            let Component::Normal(name) = component else {
                return Err(denied());
            };
            dir = dir
                .open_dir_nofollow(name)
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => Error::new(ErrorKind::NotFound),
                    _ => denied(),
                })?;
        }
        Ok((dir, name))
    }
}

fn relative(path: &str) -> Result<PathBuf, Error> {
    if path.is_empty()
        || path.len() > MAX_PATH
        || path.chars().any(char::is_control)
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(denied());
    }
    Ok(PathBuf::from(path))
}

fn denied() -> Error {
    Error {
        kind: ErrorKind::PermissionRequired,
        reason: Some("outside_workspace"),
    }
}

fn io_failure(error: std::io::Error) -> Error {
    use std::io::ErrorKind as Io;
    let kind = match error.kind() {
        Io::NotFound => ErrorKind::NotFound,
        Io::PermissionDenied => ErrorKind::PermissionRequired,
        _ => ErrorKind::ExternalFailure,
    };
    Error {
        kind,
        reason: Some("workspace_io"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temp.path().to_str().unwrap()).unwrap();
        (temp, workspace)
    }

    #[test]
    fn reads_and_writes_inside_the_root() {
        let (temp, workspace) = workspace();
        std::fs::create_dir(temp.path().join("notes")).unwrap();
        workspace.write("notes/a.txt", "one\ntwo\nthree").unwrap();
        let read = workspace.read("notes/a.txt", None, None).unwrap();
        assert_eq!(read.content, "one\ntwo\nthree");
        assert!(!read.truncated);
        assert_eq!(
            workspace
                .read("notes/a.txt", Some(2), Some(1))
                .unwrap()
                .content,
            "two\n"
        );
        assert!(
            workspace
                .read("notes/a.txt", Some(4), None)
                .unwrap()
                .content
                .is_empty()
        );
        // A rewrite replaces the file through the same directory handle.
        workspace.write("notes/a.txt", "four").unwrap();
        assert_eq!(
            workspace.read("notes/a.txt", None, None).unwrap().content,
            "four"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_windows_file_symlink() {
        let (temp, workspace) = workspace();
        std::fs::write(temp.path().join("real"), "x").unwrap();
        std::os::windows::fs::symlink_file(temp.path().join("real"), temp.path().join("link"))
            .unwrap();
        assert!(workspace.read("link", None, None).is_err());
        assert!(workspace.write("link", "x").is_err());
        assert!(
            workspace
                .directory(temp.path().join("link").to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn an_uninspectable_component_is_denied() {
        let (temp, workspace) = workspace();
        let missing = temp.path().join("missing-component").join("child");
        assert!(workspace.directory(missing.to_str().unwrap()).is_err());
        assert!(
            workspace
                .read("missing-component/child", None, None)
                .is_err()
        );
    }

    #[test]
    fn rejects_escapes_and_symlinks() {
        let (temp, workspace) = workspace();
        assert!(workspace.read("../escape", None, None).is_err());
        assert!(workspace.read("/etc/passwd", None, None).is_err());
        assert!(workspace.write("a/../../escape", "x").is_err());
        std::fs::create_dir_all(temp.path().join("dir")).unwrap();
        std::fs::write(temp.path().join("dir/real"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("dir/real"), temp.path().join("link")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path().join("dir"), temp.path().join("dirlink"))
                .unwrap();
            assert!(workspace.read("link", None, None).is_err());
            assert!(workspace.write("link", "x").is_err());
            assert!(workspace.read("dirlink/real", None, None).is_err());
            assert!(workspace.write("dirlink/new", "x").is_err());
        }
        assert!(workspace.write("dir", "x").is_err());
        assert!(workspace.write("missing/target", "x").is_err());
        assert!(workspace.directory("..").is_err());
        assert!(
            workspace
                .directory(temp.path().join("dir").to_str().unwrap())
                .is_ok()
        );
        #[cfg(unix)]
        assert!(
            workspace
                .directory(temp.path().join("dirlink").to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn enforces_size_and_line_limits() {
        let (_temp, workspace) = workspace();
        assert!(
            workspace
                .write("big", &"x".repeat(MAX_WRITE_BYTES + 1))
                .is_err()
        );
        workspace.write("ok", "x").unwrap();
        assert!(workspace.read("ok", Some(0), None).is_err());
        assert!(workspace.read("ok", None, Some(MAX_LINES + 1)).is_err());
        let mut content = String::new();
        for index in 0..(MAX_READ_BYTES / 2) {
            content.push_str(&format!("{index}\n"));
        }
        assert!(workspace.write("large", &content).is_err());
    }
}
