//! Workspace-scoped text file access for host-mediated tools.
//!
//! Paths are relative to a canonical workspace root. Absolute paths, `..`,
//! `.`, symlinks and non-regular files are rejected; writes are atomic and
//! never create directories implicitly.
use crate::domain::{Error, ErrorKind};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Component, Path, PathBuf},
};

const MAX_PATH: usize = 4096;
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_WRITE_BYTES: usize = 256 * 1024;
const MAX_LINES: u32 = 10_000;

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        let strip = |path: &std::path::Path| {
            path.to_string_lossy()
                .strip_prefix(r"\\?\")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| path.to_path_buf())
        };
        return strip(left) == strip(right);
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Workspace {
    root: PathBuf,
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
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical directory inside the workspace; symlinks and escapes are rejected.
    pub(crate) fn directory(&self, path: &str) -> Result<PathBuf, Error> {
        let path = Path::new(path);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(io_failure)?;
        if !same_path(&canonical, &candidate)
            || !canonical.starts_with(&self.root)
            || !canonical.is_dir()
        {
            return Err(denied());
        }
        Ok(canonical)
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
        let file = self.existing(&relative)?;
        let content = std::fs::read_to_string(&file).map_err(io_failure)?;
        if content.len() > MAX_READ_BYTES {
            return Err(Error::new(ErrorKind::ResourceLimit));
        }
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
        let target = self.root.join(&relative);
        let parent = target.parent().ok_or_else(Error::invalid)?;
        let canonical = std::fs::canonicalize(parent).map_err(io_failure)?;
        if !same_path(&canonical, parent) || !canonical.starts_with(&self.root) {
            return Err(denied());
        }
        if target.exists() {
            let metadata = std::fs::symlink_metadata(&target).map_err(io_failure)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(denied());
            }
        }
        let tmp = parent.join(format!(".ene-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
            }
            let mut file = options.open(&tmp).map_err(io_failure)?;
            file.write_all(content.as_bytes()).map_err(io_failure)?;
            file.sync_all().map_err(io_failure)?;
            std::fs::rename(&tmp, &target).map_err(io_failure)
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    fn existing(&self, relative: &Path) -> Result<PathBuf, Error> {
        let lexical = self.root.join(relative);
        let canonical =
            std::fs::canonicalize(&lexical).map_err(|_| Error::new(ErrorKind::NotFound))?;
        if !same_path(&canonical, &lexical) || !canonical.starts_with(&self.root) {
            return Err(denied());
        }
        let metadata = std::fs::symlink_metadata(&canonical).map_err(io_failure)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(denied());
        }
        Ok(canonical)
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
            assert!(workspace.read("dirlink/real", None, None).is_err());
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
