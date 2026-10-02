//! No unrestricted fallback. Each instance has private PID/mount/network namespaces.
use super::manifest::{Bundle, load};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use std::{path::Path, process::Stdio};
use tokio::process::{Child, Command};
pub fn spawn(bundle: &Bundle) -> Result<Child> {
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
    let mut cmd = Command::new("/usr/bin/bwrap");
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
        .arg(&bundle.root)
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
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::new(ErrorKind::Unavailable))
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
