'use strict';
const fs = require('fs');
const path = require('path');
const { createRequire } = require('module');

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

function resolveDirectTarget(cmd, cwd, env) {
  if (cmd.includes('\\') || cmd.includes('/')) {
    const absolute = path.resolve(cwd, cmd);
    return resolveInDir(path.dirname(absolute), path.basename(absolute), env).target;
  }
  // Arbitrary shell expressions still belong to npm's selected shell.
  if (/[\s&|<>^();!%"'\r\n]/.test(cmd)) return null;
  const dirs = getPathValue(env).split(path.delimiter).filter(Boolean);
  const seen = new Set();
  for (const dir of dirs) {
    const key = path.resolve(dir).toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    const result = resolveInDir(dir, cmd, env);
    if (result.found) return result.target;
  }
  return null;
}

function isDefaultWindowsShell(shell, env) {
  if (!shell || shell === true) return true;
  if (typeof shell !== 'string') return false;
  const base = path.basename(shell.trim()).toLowerCase();
  if (base === 'cmd' || base === 'cmd.exe') return true;
  const key = Object.keys(env).find((name) => name.toLowerCase() === 'comspec');
  return Boolean(key && shell.trim().toLowerCase() === String(env[key]).trim().toLowerCase());
}

function withShimEnvironment(env, extra) {
  const result = { ...env };
  for (const [name, value] of Object.entries(extra)) {
    const key = Object.keys(result).find((key) => key.toLowerCase() === name.toLowerCase());
    result[key || name] = value;
  }
  const pathKey = Object.keys(result).find((key) => key.toLowerCase() === 'path') || 'PATH';
  result[pathKey] = [result[pathKey], path.dirname(process.execPath)].filter(Boolean).join(path.delimiter);
  return result;
}

function installDirectSpawn(npx) {
  // Resolve through this installation of npm. Its run-script has already
  // assembled npm's PATH, package variables, working directory and signals.
  const npmRequire = createRequire(npx);
  const execRequire = createRequire(npmRequire.resolve('libnpmexec'));
  const runRequire = createRequire(execRequire.resolve('@npmcli/run-script'));
  const spawnFile = runRequire.resolve('@npmcli/promise-spawn');
  const spawn = runRequire(spawnFile);
  require.cache[spawnFile].exports = Object.assign(function (cmd, args, options, extra) {
    if (!options || !extra || extra.event !== 'npx') return spawn(cmd, args, options, extra);
    const env = options.env || process.env;
    if ((options.stdio === undefined || options.stdio === 'inherit') &&
        isDefaultWindowsShell(options.shell, env)) {
      const cwd = options.cwd || process.cwd();
      const target = resolveDirectTarget(cmd, cwd, env);
      if (target) {
        return spawn(target.file, [...target.prefixArgs, ...args], {
          ...options,
          env: withShimEnvironment(env, target.extraEnv),
          shell: false,
          windowsHide: true,
        }, extra);
      }
    }
    return spawn(cmd, args, options, extra);
  }, spawn);
}

const npx = path.resolve(process.argv[1]);
if (process.platform === 'win32') {
  try {
    installDirectSpawn(npx);
  } catch (error) {
    // Other npm layouts can still run through the regular CLI.
    if (error.code !== 'MODULE_NOT_FOUND') throw error;
  }
}
require(npx);
