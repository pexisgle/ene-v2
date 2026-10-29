//! OS-enforced isolation for trusted, host-owned worker executables.
use std::{io, path::Path};
#[cfg(windows)]
#[path = "sandbox_windows.rs"]
mod os;
#[cfg(windows)]
pub use os::{Child, ChildStdin, ChildStdout, spawn};
#[cfg(unix)]
pub use tokio::process::{Child, ChildStdin, ChildStdout};

/// Launch a trusted host executable with a read-only package and private temporary storage.
/// The plugin runtime chooses the Rust runner; manifests cannot supply executables or args.
/// `/plugin` in arguments denotes the isolated package root on either OS.
#[cfg(unix)]
pub fn spawn(root: &Path, executable: &Path, args: &[String]) -> io::Result<Child> {
    use std::process::Stdio;
    let binary = std::fs::canonicalize(executable)?;
    if !binary.is_file() {
        return Err(io::Error::other("missing worker executable"));
    }
    // Resolve before clearing the environment; Nix does not install bwrap in /usr/bin.
    let bwrap = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("bwrap"))
        .find(|path| path.is_absolute() && path.is_file())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bubblewrap is required"))?;
    let mut cmd = tokio::process::Command::new(bwrap);
    cmd.env_clear().args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]);
    for path in ["/usr", "/lib", "/lib64", "/bin", "/nix/store"] {
        if Path::new(path).exists() {
            cmd.args(["--ro-bind", path, path]);
        }
    }
    cmd.arg("--ro-bind")
        .arg(root)
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
            "--dir",
            "/ene-runtime",
            "--ro-bind",
        ])
        .arg(binary)
        .arg("/ene-runtime/runner")
        .args(["--", "/ene-runtime/runner"])
        .args(args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}
