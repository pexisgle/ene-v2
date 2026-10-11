//! Both plugin protocols use the same OS-enforced process boundary.
use std::{io, path::Path};
#[cfg(windows)]
#[path = "sandbox_windows.rs"]
mod os;
#[cfg(windows)]
pub use os::{Child, ChildStdin, ChildStdout, spawn};
#[cfg(unix)]
pub use tokio::process::{Child, ChildStdin, ChildStdout};

/// `/plugin/` is a logical package root, not a path into the host filesystem.
pub fn command_valid(command: &[String]) -> bool {
    !command.is_empty()
        && command.iter().all(|a| a.len() <= 4096 && !a.contains('\0'))
        && (command[0]
            .strip_prefix("/plugin/")
            .is_some_and(super::relative)
            || command[0]
                .strip_prefix("/usr/bin/")
                .is_some_and(super::relative))
        && command
            .iter()
            .all(|arg| arg.strip_prefix("/plugin/").is_none_or(super::relative))
}

/// Discovery accepts portable manifests; enabling requires a command for this OS.
pub fn command_supported(command: &[String]) -> bool {
    command_valid(command)
        && (cfg!(unix)
            || command[0].strip_prefix("/plugin/").is_some_and(|path| {
                Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            }))
}

#[cfg(unix)]
pub fn spawn(root: &Path, command: &[String], runner: Option<(&Path, &str)>) -> io::Result<Child> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("/usr/bin/bwrap");
    cmd.env_clear().args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]);
    for path in ["/usr", "/lib", "/lib64", "/bin"] {
        if Path::new(path).exists() {
            cmd.args(["--ro-bind", path, path]);
        }
    }
    cmd.arg("--ro-bind").arg(root).arg("/plugin").args([
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
    ]);
    if let Some((binary, entrypoint)) = runner {
        let binary = std::fs::canonicalize(binary)?;
        if !binary.is_file() {
            return Err(io::Error::other("missing plugin runner"));
        }
        cmd.args(["--dir", "/ene-runtime", "--ro-bind"])
            .arg(binary)
            .arg("/ene-runtime/runner")
            .args([
                "--",
                "/ene-runtime/runner",
                "--package",
                "/plugin",
                "--entrypoint",
                entrypoint,
            ]);
    } else {
        if !command_supported(command) {
            return Err(io::Error::other("unsupported plugin command"));
        }
        cmd.arg("--").args(command);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}
