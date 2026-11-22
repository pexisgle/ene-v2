'use strict';
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
