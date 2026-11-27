//! Windows-safe `npx` resolution for stdio MCP servers.

#[cfg(windows)]
use super::{stdio::owned_command, transport::ConnectFailure};
#[cfg(windows)]
use ene_domain::settings::McpConnection;
pub use ene_domain::settings::is_npx_command;
#[cfg(windows)]
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(windows)]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;
#[cfg(windows)]
const NPX_DIRECT_BOOTSTRAP_JS: &str = include_str!("npx_bootstrap.js");

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum NpxLaunch {
    NodeCli {
        node_exe: PathBuf,
        npx_cli_js: PathBuf,
    },
    DirectExecutable(PathBuf),
}

/// Builds a `tokio::process::Command` for an `npx` MCP server on Windows
/// without executing `npx.cmd` or `npx.ps1`.
#[cfg(windows)]
pub(crate) async fn build_command(
    want: &McpConnection,
) -> Result<tokio::process::Command, ConnectFailure> {
    let cwd = want.cwd.trim();
    if !cwd.is_empty() && !Path::new(cwd).is_dir() {
        return Err(ConnectFailure::Configuration(
            "the MCP working directory does not exist".into(),
        ));
    }
    let path_var = effective_env(&want.env, "PATH");
    let appdata = effective_env(&want.env, "APPDATA");
    let path_dirs: Vec<PathBuf> = path_var
        .as_deref()
        .map_or_else(Vec::new, |val| std::env::split_paths(val).collect());

    let launch = resolve_launch(
        want.command.trim(),
        &path_dirs,
        appdata.as_deref().map(Path::new),
        cwd,
        &want.env,
    )
    .await?;

    let mut command = match launch {
        NpxLaunch::NodeCli {
            node_exe,
            npx_cli_js,
        } => {
            let mut cmd = tokio::process::Command::new(&node_exe);
            cmd.arg("-e");
            cmd.arg(NPX_DIRECT_BOOTSTRAP_JS);
            cmd.arg("--");
            cmd.arg(&npx_cli_js);
            cmd
        }
        NpxLaunch::DirectExecutable(program) => tokio::process::Command::new(program),
    };
    command.args(&want.args);
    Ok(command)
}

#[cfg(windows)]
fn effective_env(overrides: &BTreeMap<String, String>, name: &str) -> Option<OsString> {
    if let Some((_, value)) = overrides
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        return Some(OsString::from(value));
    }
    std::env::var_os(name)
}

#[cfg(windows)]
async fn resolve_launch(
    raw_command: &str,
    path_dirs: &[PathBuf],
    appdata: Option<&Path>,
    cwd: &str,
    env: &BTreeMap<String, String>,
) -> Result<NpxLaunch, ConnectFailure> {
    let path = Path::new(raw_command.trim());
    if path.parent().is_some_and(|dir| !dir.as_os_str().is_empty()) {
        return resolve_explicit(path, cwd, path_dirs);
    }

    let mut npx_dirs: Vec<PathBuf> = Vec::new();
    for dir in path_dirs {
        if has_any_file(dir, &["npx.cmd", "npx.ps1", "npx.exe", "npx"]) {
            push_unique_dir(&mut npx_dirs, dir.clone());
        }
    }

    let mut node_exe = find_file_in_dirs(&npx_dirs, "node.exe")
        .or_else(|| find_file_in_dirs(path_dirs, "node.exe"));

    if let Some(ref mut node) = node_exe {
        let mut candidate_dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = node.parent() {
            push_unique_dir(&mut candidate_dirs, dir.to_path_buf());
        }
        for dir in &npx_dirs {
            push_unique_dir(&mut candidate_dirs, dir.clone());
        }

        if find_bundled_npx_cli(&candidate_dirs).is_none()
            && let Some(real_node) = probe_node_exec_path(node, cwd, env).await
        {
            if let Some(real_dir) = real_node.parent() {
                push_unique_dir(&mut candidate_dirs, real_dir.to_path_buf());
            }
            *node = real_node;
        }

        if let Some(prefix_js) = find_bundled_npm_prefix(&candidate_dirs)
            && let Some(global_prefix) = probe_npm_prefix(node, &prefix_js, cwd, env).await
        {
            let prefix_cli = npm_npx_cli_in(&global_prefix);
            if prefix_cli.is_file() {
                return Ok(NpxLaunch::NodeCli {
                    node_exe: node.clone(),
                    npx_cli_js: prefix_cli,
                });
            }
        }

        if let Some(cli) = find_bundled_npx_cli(&candidate_dirs) {
            return Ok(NpxLaunch::NodeCli {
                node_exe: node.clone(),
                npx_cli_js: cli,
            });
        }

        if let Some(appdata_dir) = appdata {
            let user_cli = npm_npx_cli_in(&appdata_dir.join("npm"));
            if user_cli.is_file() {
                return Ok(NpxLaunch::NodeCli {
                    node_exe: node.clone(),
                    npx_cli_js: user_cli,
                });
            }
        }
    }

    if let Some(npx_exe) = find_file_in_dirs(path_dirs, "npx.exe") {
        return Ok(NpxLaunch::DirectExecutable(npx_exe));
    }

    if let Some(npx_cmd) = find_file_in_dirs(path_dirs, "npx.cmd") {
        return Ok(NpxLaunch::DirectExecutable(npx_cmd));
    }

    let reason = if let Some(node) = node_exe {
        format!(
            "npx-cli.js was not found for Node.js at `{}` (and neither npx.exe nor npx.cmd was found on PATH)",
            node.display()
        )
    } else {
        "node.exe (and npx) was not found on PATH".to_owned()
    };
    Err(ConnectFailure::Configuration(reason))
}

#[cfg(windows)]
fn resolve_explicit(
    path: &Path,
    cwd: &str,
    path_dirs: &[PathBuf],
) -> Result<NpxLaunch, ConnectFailure> {
    let missing =
        || ConnectFailure::Configuration("the specified npx launcher was not found".into());
    let path = std::path::absolute(Path::new(cwd).join(path)).map_err(|_| missing())?;
    let parent = path.parent().ok_or_else(missing)?;
    let program = if path.extension().is_some() {
        path.is_file().then(|| path.clone())
    } else {
        ["exe", "cmd", "ps1"]
            .into_iter()
            .map(|extension| path.with_extension(extension))
            .find(|file| file.is_file())
    }
    .ok_or_else(missing)?;
    let extension = program.extension().unwrap_or_default();
    if extension.eq_ignore_ascii_case("exe") {
        return Ok(NpxLaunch::DirectExecutable(program));
    }
    let npx_cli_js = npm_npx_cli_in(parent);
    let node = parent.join("node.exe");
    if npx_cli_js.is_file()
        && let Some(node_exe) = node
            .is_file()
            .then_some(node)
            .or_else(|| find_file_in_dirs(path_dirs, "node.exe"))
    {
        return Ok(NpxLaunch::NodeCli {
            node_exe,
            npx_cli_js,
        });
    }
    if extension.eq_ignore_ascii_case("cmd") {
        return Ok(NpxLaunch::DirectExecutable(program));
    }
    Err(ConnectFailure::Configuration(
        "the specified npx PowerShell wrapper has no adjacent npm installation".into(),
    ))
}

#[cfg(windows)]
fn push_unique_dir(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs
        .iter()
        .any(|existing| existing.as_os_str().eq_ignore_ascii_case(dir.as_os_str()))
    {
        dirs.push(dir);
    }
}

#[cfg(windows)]
fn has_any_file(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| dir.join(name).is_file())
}

#[cfg(windows)]
fn find_file_in_dirs(dirs: &[PathBuf], file_name: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn npm_npx_cli_in(root: &Path) -> PathBuf {
    root.join("node_modules")
        .join("npm")
        .join("bin")
        .join("npx-cli.js")
}

#[cfg(windows)]
fn npm_prefix_js_in(root: &Path) -> PathBuf {
    root.join("node_modules")
        .join("npm")
        .join("bin")
        .join("npm-prefix.js")
}

#[cfg(windows)]
fn find_bundled_npx_cli(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| npm_npx_cli_in(dir))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn find_bundled_npm_prefix(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| npm_prefix_js_in(dir))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
async fn run_node_probe(
    node_exe: &Path,
    args: &[&std::ffi::OsStr],
    cwd: &str,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    use tokio::io::AsyncReadExt as _;
    let mut cmd = tokio::process::Command::new(node_exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    cmd.envs(env);
    let mut child = owned_command(cmd).spawn().ok()?;
    let mut stdout = child.stdout().take()?.take(PROBE_OUTPUT_LIMIT + 1);
    let mut output = Vec::new();
    let (status, bytes) = tokio::time::timeout(PROBE_TIMEOUT, async {
        tokio::try_join!(child.wait(), stdout.read_to_end(&mut output))
    })
    .await
    .ok()?
    .ok()?;
    (status.success() && bytes as u64 <= PROBE_OUTPUT_LIMIT)
        .then(|| String::from_utf8_lossy(&output))?
        .lines()
        .find_map(|line| (!line.trim().is_empty()).then(|| PathBuf::from(line.trim())))
}

#[cfg(windows)]
async fn probe_node_exec_path(
    node_exe: &Path,
    cwd: &str,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let resolved = run_node_probe(
        node_exe,
        &[
            std::ffi::OsStr::new("-p"),
            std::ffi::OsStr::new("process.execPath"),
        ],
        cwd,
        env,
    )
    .await?;
    resolved.is_file().then_some(resolved)
}

#[cfg(windows)]
async fn probe_npm_prefix(
    node_exe: &Path,
    prefix_js: &Path,
    cwd: &str,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let prefix = run_node_probe(node_exe, &[prefix_js.as_os_str()], cwd, env).await?;
    prefix.is_dir().then_some(prefix)
}

#[cfg(test)]
#[path = "npx_tests.rs"]
mod tests;
