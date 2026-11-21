//! Windows-safe `npx` resolution for stdio MCP servers.
//!
//! On Windows, `npx` is normally installed as batch/PowerShell wrappers
//! (`npx.cmd` / `npx.ps1`) and `npm exec` launches package bins through
//! `cmd.exe` and generated `node_modules/.bin/*.cmd` shims. In locked-down
//! environments where script execution (`.cmd`, `.bat`, `.ps1`, or `cmd.exe`)
//! is blocked by policy or permissions, invoking `npx.cmd` fails even when
//! `node.exe` itself is allowed.
//!
//! Instead of spawning `npx.cmd`, this module locates `node.exe` and
//! `npx-cli.js` directly and runs a small Node.js bootstrap that intercepts
//! `npx`'s final `@npmcli/run-script` bin launch to execute the target JS
//! entrypoint (via `process.execPath`) or native `.exe` directly with
//! `shell: false`. Complex shell expressions still fall back to npm's
//! original runner.

#[cfg(windows)]
use super::supervisor::{ConnectFailure, Desired};
#[cfg(windows)]
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns true when `command` refers to `npx` (bare name or path, with or
/// without a Windows wrapper extension).
pub(crate) fn is_npx_command(command: &str) -> bool {
    let base = command_basename(command.trim());
    ["npx", "npx.cmd", "npx.ps1", "npx.exe"]
        .iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

fn command_basename(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum NpxLaunch {
    /// Launch `node.exe` with the direct bootstrap + `npx-cli.js` (no `.cmd`
    /// wrapper or `cmd.exe` needed).
    NodeCli {
        node_exe: PathBuf,
        npx_cli_js: PathBuf,
    },
    /// Fallback when `npx-cli.js` cannot be located (e.g. standalone shim).
    DirectExecutable(PathBuf),
}

/// Small CommonJS bootstrap run via `node.exe -e <src> -- <npx-cli.js> <args...>`.
///
/// Node populates `process.argv` as `[node_exe, npx_cli_js, ...args]`. Before
/// loading `npx-cli.js`, the bootstrap wraps `@npmcli/run-script` so that when
/// `libnpmexec` runs the resolved MCP bin, Node parses the generated `.cmd` /
/// POSIX shim as plain text and spawns `process.execPath` (for Node scripts)
/// or the target `.exe` directly with `shell: false` — never invoking
/// `cmd.exe` or executing `.cmd` files.
///
/// Must never write to `stdout`, because MCP stdio speaks JSON-RPC on stdout.
#[cfg(windows)]
const NPX_DIRECT_BOOTSTRAP_JS: &str = r#"'use strict';
const fs = require('fs');
const path = require('path');
const Module = require('module');
const cp = require('child_process');

const wrapped = new WeakMap();

function splitSimpleArgs(str) {
  const out = [];
  let cur = '';
  let quote = null;
  for (let i = 0; i < str.length; i++) {
    const ch = str[i];
    if (quote) {
      if (ch === quote) quote = null;
      else cur += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (/\s/.test(ch)) {
      if (cur.length > 0) {
        out.push(cur);
        cur = '';
      }
    } else {
      cur += ch;
    }
  }
  if (quote) return null;
  if (cur.length > 0) out.push(cur);
  return out;
}

function isFile(p) {
  try {
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

function isNodeProgram(name) {
  const unquoted = name.trim().replace(/^"(.*)"$/, '$1');
  const base = path.basename(unquoted).toLowerCase();
  return base === 'node' || base === 'node.exe';
}

function expandBatchEnv(val, env) {
  return val.replace(/%([^%]+)%/g, (match, name) => {
    if (!name) return '%';
    const lower = name.toLowerCase();
    for (const k of Object.keys(env)) {
      if (k.toLowerCase() === lower) return env[k] || '';
    }
    return '';
  });
}

function parseCmdShim(cmdPath, baseEnv) {
  let text;
  try {
    text = fs.readFileSync(cmdPath, 'utf8');
  } catch {
    return null;
  }
  const dir = path.dirname(cmdPath);
  const extraEnv = {};
  let prog = null;
  const lines = text.split(/\r?\n/);
  for (const raw of lines) {
    const line = raw.trim();
    const quoted = /^@?SET\s+"([^"=]+)=([^"]*)"$/i.exec(line);
    const bare = !quoted ? /^@?SET\s+([^"=\s]+)=(.*)$/i.exec(line) : null;
    const match = quoted || bare;
    if (match) {
      const key = match[1].trim();
      const val = match[2];
      const lower = key.toLowerCase();
      if (lower === '_prog') {
        prog = val;
      } else if (lower !== 'dp0' && lower !== 'pathext') {
        extraEnv[key] = expandBatchEnv(val, baseEnv);
      }
    }
  }

  // Modern cmd-shim: ... & "%_prog%" <extraArgs> "%dp0%\<target>" %*
  const modern = /"%_prog%"\s+(.*?)"%(?:~?dp0)%(?:\\|\/)*([^"]+)"\s+%\*/i.exec(text);
  if (modern && prog && isNodeProgram(prog)) {
    const extraArgs = splitSimpleArgs(modern[1].trim());
    if (!extraArgs) return null;
    const rel = modern[2].replace(/^[\\/]+/, '');
    const target = path.resolve(dir, rel);
    if (isFile(target)) {
      return { file: process.execPath, prefixArgs: [...extraArgs, target], extraEnv };
    }
  }

  // Legacy cmd-shim: "%~dp0\node.exe" <extraArgs> "%~dp0\<target>" %*
  const legacy = /"%~?dp0%(?:\\|\/)node\.exe"\s+(.*?)"%~?dp0%(?:\\|\/)*([^"]+)"\s+%\*/i.exec(text);
  if (legacy) {
    const extraArgs = splitSimpleArgs(legacy[1].trim());
    if (!extraArgs) return null;
    const rel = legacy[2].replace(/^[\\/]+/, '');
    const target = path.resolve(dir, rel);
    if (isFile(target)) {
      return { file: process.execPath, prefixArgs: [...extraArgs, target], extraEnv };
    }
  }

  // Direct binary cmd-shim (no shebang): "%dp0%\<target.exe>" %*
  for (const raw of lines) {
    const direct = /^\s*"%(?:~?dp0)%(?:\\|\/)*([^"]+)"\s+%\*\s*$/i.exec(raw);
    if (direct) {
      const rel = direct[1].replace(/^[\\/]+/, '');
      const target = path.resolve(dir, rel);
      if (isFile(target) && /\.(?:exe|com)$/i.test(target)) {
        return { file: target, prefixArgs: [], extraEnv };
      }
    }
  }
  return null;
}

function parsePosixShimOrScript(filePath) {
  let text;
  try {
    text = fs.readFileSync(filePath, 'utf8');
  } catch {
    return null;
  }
  const dir = path.dirname(filePath);
  const firstLine = (text.split(/\r?\n/, 1)[0] || '').trim();
  const nodeShebang = /^#!\s*(?:\/usr\/bin\/env\s+(?:-S\s+)?)?(?:\S*\/)?node(?:\.exe)?(?:\s+(.*))?$/i.exec(firstLine);
  if (nodeShebang) {
    const extraArgs = splitSimpleArgs((nodeShebang[1] || '').trim());
    if (!extraArgs) return null;
    return { file: process.execPath, prefixArgs: [...extraArgs, filePath], extraEnv: {} };
  }
  const shMatch = /exec\s+((?:[^\s=]+=\S+\s+)*)(?:"\$basedir\/node(?:\.exe)?"|node(?:\.exe)?)\s+(.*?)"\$basedir\/([^"]+)"\s+"\$@"/i.exec(text);
  if (shMatch) {
    const extraEnv = {};
    if (shMatch[1]) {
      for (const pair of shMatch[1].trim().split(/\s+/)) {
        const eq = pair.indexOf('=');
        if (eq > 0) extraEnv[pair.slice(0, eq)] = pair.slice(eq + 1);
      }
    }
    const extraArgs = splitSimpleArgs(shMatch[2].trim());
    if (!extraArgs) return null;
    const rel = shMatch[3].replace(/^[\\/]+/, '');
    const target = path.resolve(dir, rel);
    if (isFile(target)) {
      return { file: process.execPath, prefixArgs: [...extraArgs, target], extraEnv };
    }
  }
  return null;
}

function resolveInDir(dir, cmd, baseEnv) {
  const lower = cmd.toLowerCase();
  const exePath = lower.endsWith('.exe') ? path.join(dir, cmd) : path.join(dir, cmd + '.exe');
  if (isFile(exePath)) {
    return { found: true, target: { file: exePath, prefixArgs: [], extraEnv: {} } };
  }
  const cmdPath = lower.endsWith('.cmd') ? path.join(dir, cmd) : path.join(dir, cmd + '.cmd');
  if (isFile(cmdPath)) {
    const parsed = parseCmdShim(cmdPath, baseEnv);
    if (parsed) return { found: true, target: parsed };
    // Try sibling POSIX shim generated alongside .cmd before giving up.
    const barePath = lower.endsWith('.cmd') ? cmdPath.slice(0, -4) : path.join(dir, cmd);
    if (isFile(barePath)) {
      const posix = parsePosixShimOrScript(barePath);
      if (posix) return { found: true, target: posix };
    }
    return { found: true, target: null };
  }
  const barePath = path.join(dir, cmd);
  if (isFile(barePath)) {
    const posix = parsePosixShimOrScript(barePath);
    if (posix) return { found: true, target: posix };
    return { found: true, target: null };
  }
  return { found: false, target: null };
}

function getPathValue(env) {
  for (const k of Object.keys(env)) {
    if (k.toLowerCase() === 'path') return env[k] || '';
  }
  return '';
}

function resolveDirectTarget(cmd, opts, baseEnv) {
  const hasSep = cmd.includes('\\') || cmd.includes('/');
  if (hasSep) {
    const abs = path.resolve(opts.path || process.cwd(), cmd);
    const dir = path.dirname(abs);
    const name = path.basename(abs);
    return resolveInDir(dir, name, baseEnv).target;
  }
  // Reject shell pipelines/expressions such as `npx -c "a && b"`.
  if (/[\s&|<>^();!%"'\r\n]/.test(cmd)) {
    return null;
  }
  const dirs = [];
  if (Array.isArray(opts.binPaths)) {
    for (const d of opts.binPaths) {
      if (d) dirs.push(d);
    }
  }
  if (opts.path) {
    dirs.push(path.join(opts.path, 'node_modules', '.bin'));
  }
  const pathVal = getPathValue(baseEnv);
  if (pathVal) {
    for (const d of pathVal.split(path.delimiter)) {
      if (d) dirs.push(d);
    }
  }
  const seen = new Set();
  for (const dir of dirs) {
    const key = path.resolve(dir).toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    const res = resolveInDir(dir, cmd, baseEnv);
    if (res.found) return res.target;
  }
  return null;
}

function isDefaultWindowsShell(shell) {
  if (!shell || shell === true) return true;
  if (typeof shell !== 'string') return false;
  const base = path.basename(shell.trim()).toLowerCase();
  if (base === 'cmd' || base === 'cmd.exe') return true;
  const comspecKey = Object.keys(process.env).find((k) => k.toLowerCase() === 'comspec');
  const comspec = comspecKey ? process.env[comspecKey] : '';
  return Boolean(comspec && shell.trim().toLowerCase() === comspec.trim().toLowerCase());
}

function buildChildEnv(opts, extraEnv) {
  const env = Object.assign({}, process.env, opts.env || {}, extraEnv || {});
  const pathKeys = Object.keys(env).filter((k) => k.toLowerCase() === 'path');
  const primaryKey = pathKeys[0] || 'PATH';
  const existing = [];
  for (const k of pathKeys) {
    if (env[k]) existing.push(env[k]);
    if (k !== primaryKey) delete env[k];
  }
  const prefix = [];
  if (Array.isArray(opts.binPaths)) {
    for (const d of opts.binPaths) {
      if (d) prefix.push(d);
    }
  }
  prefix.push(path.dirname(process.execPath));
  env[primaryKey] = [...prefix, ...existing].filter(Boolean).join(path.delimiter);
  env.npm_lifecycle_event = opts.event;
  if (opts.cmd || (opts.pkg && opts.pkg.scripts && opts.pkg.scripts[opts.event])) {
    env.npm_lifecycle_script = opts.cmd || opts.pkg.scripts[opts.event];
  }
  return env;
}

function wrapRunScript(orig) {
  if (wrapped.has(orig)) return wrapped.get(orig);
  const directRunScript = function (opts) {
    if (
      process.platform === 'win32' &&
      opts &&
      opts.event === 'npx' &&
      (opts.stdio === undefined || opts.stdio === 'inherit') &&
      isDefaultWindowsShell(opts.scriptShell)
    ) {
      const cmd = opts.cmd || (opts.pkg && opts.pkg.scripts && opts.pkg.scripts[opts.event]);
      if (typeof cmd === 'string' && cmd.trim()) {
        const baseEnv = Object.assign({}, process.env, opts.env || {});
        const target = resolveDirectTarget(cmd.trim(), opts, baseEnv);
        if (target) {
          const spawnArgs = [...target.prefixArgs, ...(Array.isArray(opts.args) ? opts.args : [])];
          const spawnEnv = buildChildEnv(opts, target.extraEnv);
          return new Promise((resolve, reject) => {
            const child = cp.spawn(target.file, spawnArgs, {
              cwd: opts.path || process.cwd(),
              env: spawnEnv,
              stdio: opts.stdio || 'inherit',
              shell: false,
              windowsHide: true,
            });
            child.on('error', (err) => {
              reject(Object.assign(err, { cmd, args: spawnArgs, event: opts.event, script: cmd, path: opts.path }));
            });
            child.on('close', (code, signal) => {
              if (code || signal) {
                reject(Object.assign(new Error('command failed'), {
                  cmd,
                  args: spawnArgs,
                  code,
                  signal,
                  stdout: null,
                  stderr: null,
                  event: opts.event,
                  script: cmd,
                  path: opts.path,
                }));
              } else {
                resolve({
                  cmd,
                  args: spawnArgs,
                  code: 0,
                  signal: null,
                  stdout: null,
                  stderr: null,
                  event: opts.event,
                  script: cmd,
                  path: opts.path,
                });
              }
            });
          });
        }
      }
    }
    return orig.apply(this, arguments);
  };
  Object.assign(directRunScript, orig);
  wrapped.set(orig, directRunScript);
  return directRunScript;
}

const origLoad = Module._load;
Module._load = function (request, parent, isMain) {
  const loaded = origLoad.apply(this, arguments);
  if (request === '@npmcli/run-script' && typeof loaded === 'function') {
    return wrapRunScript(loaded);
  }
  return loaded;
};

require(process.argv[1]);
"#;

/// Builds a `tokio::process::Command` for an `npx` MCP server on Windows
/// without executing `npx.cmd` or `npx.ps1`.
#[cfg(windows)]
pub(super) async fn build_command(
    want: &Desired,
) -> Result<tokio::process::Command, ConnectFailure> {
    let path_var = effective_env(&want.env, "PATH");
    let appdata = effective_env(&want.env, "APPDATA");
    let path_dirs: Vec<PathBuf> = path_var
        .as_deref()
        .map(|val| std::env::split_paths(val).collect())
        .unwrap_or_default();

    let launch = resolve_launch(
        &want.command,
        &path_dirs,
        appdata.as_deref().map(Path::new),
        &want.cwd,
        &want.env,
    )
    .await?;

    let mut command = match launch {
        NpxLaunch::NodeCli {
            node_exe,
            npx_cli_js,
        } => {
            let mut cmd = tokio::process::Command::new(&node_exe);
            cmd.creation_flags(CREATE_NO_WINDOW);
            cmd.arg("-e");
            cmd.arg(NPX_DIRECT_BOOTSTRAP_JS);
            cmd.arg("--");
            cmd.arg(&npx_cli_js);
            cmd
        }
        NpxLaunch::DirectExecutable(program) => {
            let mut cmd = tokio::process::Command::new(&program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            cmd
        }
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
    let trimmed = raw_command.trim();
    let explicit_path = Path::new(trimmed);
    let explicit_dir = explicit_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty());

    // 1. Collect candidate directories and locate node.exe without running any script.
    let mut npx_dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = explicit_dir {
        push_unique_dir(&mut npx_dirs, dir.to_path_buf());
    }
    for dir in path_dirs {
        if has_any_file(dir, &["npx.cmd", "npx.ps1", "npx.exe", "npx"]) {
            push_unique_dir(&mut npx_dirs, dir.clone());
        }
    }

    let mut node_exe = find_node_exe(explicit_dir, &npx_dirs, path_dirs);

    // 2. Locate npx-cli.js alongside node.exe / npx directories, resolving shim
    //    realpaths and npm's configured global prefix via node.exe (never .cmd).
    if let Some(ref mut node) = node_exe {
        let mut candidate_dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = explicit_dir {
            push_unique_dir(&mut candidate_dirs, dir.to_path_buf());
        }
        if let Some(dir) = node.parent() {
            push_unique_dir(&mut candidate_dirs, dir.to_path_buf());
        }
        for dir in &npx_dirs {
            push_unique_dir(&mut candidate_dirs, dir.clone());
        }

        // Shim managers (Scoop, Volta, mise) place a small node.exe shim in a
        // shims/ directory without a sibling node_modules/npm. Ask node.exe
        // itself for process.execPath to find the real installation directory.
        if find_bundled_npx_cli(&candidate_dirs).is_none()
            && let Some(real_node) = probe_node_exec_path(node, cwd, env).await
        {
            if let Some(real_dir) = real_node.parent() {
                push_unique_dir(&mut candidate_dirs, real_dir.to_path_buf());
            }
            *node = real_node;
        }

        // Honor a globally updated npm (e.g. `npm install -g npm`), mirroring
        // npx.cmd's `npm-prefix.js` check without invoking cmd.exe.
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

    // 3. Fallback: prefer a native `npx.exe` binary over `npx.cmd`.
    if explicit_dir.is_some()
        && explicit_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        && explicit_path.is_file()
    {
        return Ok(NpxLaunch::DirectExecutable(explicit_path.to_path_buf()));
    }
    if let Some(npx_exe) = find_file_in_dirs(path_dirs, "npx.exe") {
        return Ok(NpxLaunch::DirectExecutable(npx_exe));
    }

    // 4. Last-resort fallback for unusual custom `npx.cmd` installations that
    //    do not ship a standard `node_modules/npm/bin/npx-cli.js`.
    if explicit_dir.is_some()
        && explicit_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd"))
        && explicit_path.is_file()
    {
        return Ok(NpxLaunch::DirectExecutable(explicit_path.to_path_buf()));
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
    Err(ConnectFailure {
        reason,
        config_error: true,
    })
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
fn find_node_exe(
    explicit_dir: Option<&Path>,
    npx_dirs: &[PathBuf],
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(dir) = explicit_dir {
        let candidate = dir.join("node.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(found) = find_file_in_dirs(npx_dirs, "node.exe") {
        return Some(found);
    }
    find_file_in_dirs(path_dirs, "node.exe")
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
    let mut cmd = tokio::process::Command::new(node_exe);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    if !cwd.is_empty() && Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    if !env.is_empty() {
        cmd.envs(env);
    }
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|line| !line.trim().is_empty())?.trim();
    let path = PathBuf::from(line);
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
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
mod tests;
