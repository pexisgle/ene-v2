//! Small interactive client for ene-avatar's JSON Lines protocol.
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitCode, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const HELP: &str = "Commands (quote arguments containing spaces; backslashes are literal):
  add ID [MODEL.vrm] [MOTION.vrma|none]  add with defaults
  model ID [MODEL.vrm]   replace model (default: cached Seed-san)
  motion ID [MOTION.vrma] select motion (default: bundled idle-breath)
  motion ID none         clear the motion
  play ID | pause ID     resume or pause
  loop ID on|off         change looping
  move ID X Y            set foot position in logical pixels
  size ID PERCENT        set size (50..200)
  center ID [MONITOR]    center on a monitor (default: first)
  remove ID              remove a character
  list                  show characters, monitors and FPS
  fps NUMBER            set FPS (0..1000; 0: unlimited)
  help | quit";

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Int(#[from] std::num::ParseIntError),
    #[error("{0}")]
    Message(String),
}

impl From<&'static str> for CliError {
    fn from(message: &'static str) -> Self {
        Self::Message(message.to_owned())
    }
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Value>,
    next_id: u64,
}

impl Client {
    fn request(&mut self, mut request: Value) -> Result<Value, CliError> {
        self.next_id += 1;
        let id = self.next_id;
        request["id"] = json!(id);
        let input = self.input.as_mut().ok_or("renderer input closed")?;
        writeln!(input, "{request}").and_then(|()| input.flush())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let reply = self
                .replies
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| CliError::Message(format!("renderer response unavailable: {e}")))?;
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(CliError::Message(error.to_string()));
            }
            return reply
                .get("ok")
                .cloned()
                .ok_or_else(|| "invalid renderer response".into());
        }
    }

    fn execute(&mut self, words: &[String]) -> Result<(), CliError> {
        let args: Vec<_> = words.iter().map(String::as_str).collect();
        let request = match args.as_slice() {
            ["help"] => {
                println!("{HELP}");
                return Ok(());
            }
            ["list"] => json!({"op":"get"}),
            ["add", id, paths @ ..] if paths.len() <= 2 => {
                let model = match paths.first() {
                    Some(path) => absolute(path)?,
                    None => cached_model()?,
                };
                let motion = match paths.get(1) {
                    Some(&"none") => None,
                    Some(path) => Some(absolute(path)?),
                    None => Some(default_motion()?),
                };
                json!({"op":"add", "character":id, "config":{"model":model, "motion":motion}})
            }
            ["remove", id] => json!({"op":"remove", "character":id}),
            ["center", id] => json!({"op":"center", "character":id, "monitor":null}),
            ["center", id, monitor] => json!({"op":"center", "character":id, "monitor":monitor}),
            ["fps", value] => {
                json!({"op":"fps", "value":value.parse::<u32>()?})
            }
            _ => {
                let (id, patch) = match args.as_slice() {
                    ["model", id] => (*id, json!({"model":cached_model()?})),
                    ["model", id, path] => (*id, json!({"model":absolute(path)?})),
                    ["motion", id] => (*id, json!({"motion":default_motion()?})),
                    ["motion", id, "none"] => (*id, json!({"motion":null})),
                    ["motion", id, path] => (*id, json!({"motion":absolute(path)?})),
                    ["play", id] => (*id, json!({"playing":true})),
                    ["pause", id] => (*id, json!({"playing":false})),
                    ["loop", id, "on"] => (*id, json!({"looping":true})),
                    ["loop", id, "off"] => (*id, json!({"looping":false})),
                    ["move", id, x, y] => (*id, json!({"position":[number(x)?,number(y)?]})),
                    ["size", id, size] => (*id, json!({"size_percent":number(size)?})),
                    _ => return Err("unknown command or invalid arguments; type help".into()),
                };
                let state = self.request(json!({"op":"get"}))?;
                let mut config = state["characters"]
                    .as_array()
                    .and_then(|characters| characters.iter().find(|c| c["character"] == id))
                    .and_then(|c| c["config"].as_object())
                    .cloned()
                    .ok_or_else(|| CliError::Message(format!("unknown character: {id}")))?;
                if let Some(patch) = patch.as_object() {
                    config.extend(patch.clone());
                }
                json!({"op":"set", "character":id, "config":config})
            }
        };
        let result = self.request(request)?;
        if result.is_null() {
            println!("ok");
        } else {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Ok(())
    }
}

/// Waits for the renderer to exit, then kills it. Returns its exit status
/// when it stopped by itself before the deadline.
fn reap(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                // The child is already gone or exited on its own; `kill` then
                // fails with "not running", which is the expected case here.
                if let Err(err) = child.kill()
                    && err.kind() != std::io::ErrorKind::InvalidInput
                {
                    eprintln!("example: could not kill the child: {err}");
                }
                if let Err(err) = child.wait() {
                    eprintln!("example: could not reap the child: {err}");
                }
                return None;
            }
        }
    }
}

// Closing stdin asks the renderer to exit. Keep its stdout reader alive while
// waiting, and reap even a renderer that stopped responding.
impl Drop for Client {
    fn drop(&mut self) {
        self.input.take();
        // The exit status is not interesting here: the child is being replaced.
        let _exit_status = reap(&mut self.child, Instant::now() + Duration::from_secs(5));
    }
}

fn number(text: &str) -> Result<f32, CliError> {
    text.parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| CliError::Message(format!("expected a finite number: {text}")))
}

fn absolute(path: &str) -> Result<String, CliError> {
    std::path::absolute(path)?
        .into_os_string()
        .into_string()
        .map_err(|_| "path must be UTF-8".into())
}

fn default_motion() -> Result<String, CliError> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/vrma/idle-breath.vrma"
    );
    if !Path::new(path).is_file() {
        return Err(CliError::Message(format!(
            "bundled motion not found: {path}"
        )));
    }
    absolute(path)
}

// Keep the cache layout in sync with `ene_desktop::avatar::presets::asset_cache`.
fn cached_model() -> Result<String, CliError> {
    #[cfg(windows)]
    let cache = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?)
        .join("ene/cache");
    #[cfg(not(windows))]
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".cache"),
    }
    .join("ene");
    let path = cache.join("models/Seed-san.vrm");
    if !path.is_file() {
        return Err(CliError::Message(format!(
            "default model not found: {}; pass `model <path>` instead",
            path.display()
        )));
    }
    absolute(path.to_str().ok_or("cache path must be UTF-8")?)
}

// Deliberately not a shell: no expansion or escapes, so Windows paths keep '\'.
fn words(line: &str) -> Result<Vec<String>, CliError> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in line.chars() {
        if let Some(end) = quote {
            if c == end {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return Err("unclosed quote".into());
    }
    if started {
        result.push(word);
    }
    Ok(result)
}

fn run() -> Result<(), CliError> {
    let mut args = std::env::args_os().skip(1);
    let binary = args.next();
    if binary
        .as_deref()
        .is_some_and(|a| a == "--help" || a == "-h")
    {
        println!("Usage: control [PATH-TO-ENE-AVATAR]\n\n{HELP}");
        return Ok(());
    }
    if args.next().is_some() {
        return Err("Usage: control [PATH-TO-ENE-AVATAR]".into());
    }
    let binary = match binary {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .ok_or("cannot locate target directory")?
            .join(format!("ene-avatar{}", std::env::consts::EXE_SUFFIX)),
    };
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            CliError::Message(format!(
                "cannot start {}: {e}; build ene-avatar first",
                binary.display()
            ))
        })?;
    let input = child.stdin.take();
    let output = child.stdout.take().ok_or("renderer stdout unavailable")?;
    let (reply_tx, replies) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let message = line
                .map_err(CliError::from)
                .and_then(|line| serde_json::from_str::<Value>(&line).map_err(CliError::from));
            match message {
                Ok(value) if value.get("event").is_some() => eprintln!("event: {value}"),
                Ok(value) => {
                    if reply_tx.send(value).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("renderer output error: {error}");
                    break;
                }
            }
        }
    });
    let mut client = Client {
        child,
        input,
        replies,
        next_id: 0,
    };
    let interactive = io::stdin().is_terminal();
    let (line_tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    if interactive {
        println!("{HELP}");
    }
    loop {
        if interactive {
            print!("> ");
            io::stdout().flush()?;
        }
        let line = loop {
            if let Some(status) = client.child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(CliError::Message(format!("renderer exited: {status}")))
                };
            }
            match lines.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => break Some(line?),
                Err(RecvTimeoutError::Disconnected) => break None,
                Err(RecvTimeoutError::Timeout) => {}
            }
        };
        let parsed = line.as_deref().map(words).transpose();
        match parsed {
            Ok(None) => break,
            Ok(Some(words)) if words == ["quit"] => break,
            Ok(Some(words)) if words.is_empty() => {}
            Ok(Some(words)) => {
                if let Err(error) = client.execute(&words) {
                    eprintln!("error: {error}");
                }
            }
            Err(error) => eprintln!("error: {error}"),
        }
    }
    client.request(json!({"op":"quit"}))?;
    client.input.take();
    match reap(&mut client.child, Instant::now() + Duration::from_secs(5)) {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(CliError::Message(format!("renderer exited: {status}"))),
        None => Err("renderer did not exit after quit".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
