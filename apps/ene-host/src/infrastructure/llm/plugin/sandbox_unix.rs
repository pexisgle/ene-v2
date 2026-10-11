//! No unrestricted fallback. Each instance has private PID/mount/network namespaces.
use super::super::manifest::{Bundle, load};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};
// Retain the directory until the child has been collected, including cancellation.
pub struct Spawned {
    pub child: Child,
    pub image: tempfile::TempDir,
}
fn image(bundle: &Bundle) -> Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let image = tempfile::tempdir().map_err(|_| Error::unavailable())?;
    for file in bundle.files.iter() {
        let path = image.path().join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| Error::unavailable())?;
        }
        let Some(bytes) = &file.bytes else {
            std::fs::create_dir_all(path).map_err(|_| Error::unavailable())?;
            continue;
        };
        std::fs::write(&path, bytes).map_err(|_| Error::unavailable())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(file.mode))
            .map_err(|_| Error::unavailable())?;
    }
    for file in bundle.files.iter().rev().filter(|f| f.bytes.is_none()) {
        std::fs::set_permissions(
            image.path().join(&file.path),
            std::fs::Permissions::from_mode(file.mode),
        )
        .map_err(|_| Error::unavailable())?;
    }
    Ok(image)
}
pub fn spawn(bundle: &Bundle) -> Result<Spawned> {
    if load(&bundle.root)?.revision != bundle.revision {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let root = std::fs::canonicalize(&bundle.root).map_err(|_| Error::unavailable())?;
    let directories = xdg::BaseDirectories::new();
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.canonicalize().ok());
    let mut private = vec![
        std::env::var_os("ENE_HOST_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::infrastructure::runtime::paths::state),
    ];
    if let Ok(p) = crate::infrastructure::runtime::paths::runtime() {
        private.push(p);
    }
    if let Some(p) = directories.get_data_home() {
        private.push(p.join("keyrings"));
        private.push(p.join("kwalletd"));
    }
    let private = private
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect::<Vec<_>>();
    verify_layout(&root, home.as_deref(), &private)?;
    let image = image(bundle)?;
    // Resolve on the host before clearing its environment. Nix provides bwrap
    // through PATH, while the plugin still receives only the isolated mounts.
    let executable = bubblewrap(std::env::var_os("PATH").as_deref())?;
    let mut cmd = Command::new(executable);
    cmd.env_clear().args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]);
    for dir in ["/usr", "/lib", "/lib64", "/bin"] {
        if Path::new(dir).exists() {
            cmd.args(["--ro-bind", dir, dir]);
        }
    }
    cmd.args(["--ro-bind"])
        .arg(image.path())
        .arg("/plugin")
        .args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--chdir",
            "/plugin",
            "--",
        ])
        .args(&bundle.manifest.command);
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::new(ErrorKind::Unavailable))?;
    Ok(Spawned { child, image })
}

pub(crate) fn bubblewrap(search: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    search
        .into_iter()
        .flat_map(std::env::split_paths)
        // Do not pick a binary from the current working directory.
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("bwrap"))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(Error::unavailable)
}

fn verify_layout(root: &Path, home: Option<&Path>, private: &[std::path::PathBuf]) -> Result<()> {
    let unsafe_bundle = home.is_some_and(|home| home.starts_with(root))
        || private
            .iter()
            .any(|p| p.starts_with(root) || root.starts_with(p));
    let unsafe_runtime = ["/usr", "/lib", "/lib64", "/bin"].iter().any(|mount| {
        let base = Path::new(mount)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(mount).to_path_buf());
        home.is_some_and(|home| home.starts_with(&base))
            || private.iter().any(|p| p.starts_with(&base))
    });
    if unsafe_bundle || unsafe_runtime {
        Err(Error::new(ErrorKind::PermissionRequired))
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bubblewrap_is_resolved_from_host_path_and_missing_sandbox_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("bwrap");
        std::fs::write(&executable, "fixture").unwrap();
        let search = root.path().as_os_str();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(bubblewrap(Some(search)).is_err());
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(bubblewrap(Some(search)).unwrap(), executable);
        assert!(bubblewrap(None).is_err());
        assert!(bubblewrap(Some(std::ffi::OsStr::new(".:relative"))).is_err());
    }
    #[test]
    fn bundles_and_runtime_mounts_cannot_expose_host_private_directories() {
        let private = [std::path::PathBuf::from("/home/test/.local/state/ene")];
        assert!(
            verify_layout(
                Path::new("/home/test"),
                Some(Path::new("/home/test")),
                &private
            )
            .is_err()
        );
        assert!(verify_layout(Path::new("/home/test/.local/state"), None, &private).is_err());
        assert!(
            verify_layout(
                Path::new("/home/test/.local/share/ene/providers/plugin"),
                Some(Path::new("/home/test")),
                &private
            )
            .is_ok()
        );
        assert!(
            verify_layout(
                Path::new("/tmp/plugin"),
                None,
                &[std::path::PathBuf::from("/usr/private-host-state")]
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    fn image_uses_verified_bytes_even_if_source_is_changed_after_verification() {
        let root = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3","/plugin/provider.py"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        std::fs::write(root.path().join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(root.path().join("provider.py"), "verified implementation").unwrap();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        let bundle = load(root.path()).unwrap();
        std::fs::write(root.path().join("provider.py"), "changed implementation").unwrap();
        let image = image(&bundle).unwrap();
        assert_eq!(
            std::fs::read_to_string(image.path().join("provider.py")).unwrap(),
            "verified implementation"
        );
        assert_ne!(load(root.path()).unwrap().revision, bundle.revision);
        assert_eq!(load(image.path()).unwrap().revision, bundle.revision);
    }
}
