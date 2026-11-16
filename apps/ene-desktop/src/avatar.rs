//! The avatar renderer as a child process.
//!
//! `apps/ene-avatar` speaks JSON Lines on stdin and stdout (`control.rs`). This
//! module owns that process: it frames requests, drains replies and
//! notifications, and turns a `body.motion` operation into an `add`/`set`.
//!
//! The renderer has no network and no host connection. It knows a character id,
//! a model path and a motion path; everything else is this client's job.

use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
};

/// The renderer's own line limit (`control.rs` `read_request`).
pub const MAX_LINE: usize = 65_536;

/// A reply to one request, or a notification the renderer emitted.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Reply {
        id: u64,
        #[serde(default)]
        ok: Value,
        #[serde(default)]
        error: Option<String>,
    },
    Event(Value),
}

/// The renderer process and the channels to its stdio.
pub struct Avatar {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Result<Message, String>>,
    next_id: u64,
    /// The character each motion was last set to, so a repeated request is a no-op.
    playing: HashMap<String, String>,
}

impl Avatar {
    /// Spawns the renderer. `program` is the `ene-avatar` binary; the caller
    /// resolves it so a missing binary is an error before any model loads.
    pub fn spawn(program: &Path) -> Result<Self, String> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The renderer logs to stderr; inherit it so its diagnostics are
            // visible next to this client's own.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("{}: {error}", program.display()))?;
        let stdin = child.stdin.take().ok_or("the renderer has no stdin")?;
        let stdout = child.stdout.take().ok_or("the renderer has no stdout")?;
        let (tx, replies) = mpsc::sync_channel(64);
        thread::spawn(move || read(stdout, tx));
        Ok(Self {
            child,
            stdin,
            replies,
            next_id: 1,
            playing: HashMap::new(),
        })
    }

    /// Sends one command and returns its reply. The renderer answers in order,
    /// so a reply with another id is a protocol error rather than a reordering.
    pub fn call(&mut self, op: &str, body: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id": id, "op": op});
        if let (Some(target), Some(fields)) = (request.as_object_mut(), body.as_object()) {
            for (key, value) in fields {
                target.insert(key.clone(), value.clone());
            }
        }
        let mut line = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        line.push('\n');
        if line.len() > MAX_LINE {
            return Err("the request exceeds the renderer's 64 KiB line limit".into());
        }
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("the renderer input closed: {error}"))?;
        loop {
            match self.replies.recv() {
                Ok(Ok(Message::Reply { id: got, ok, error })) if got == id => {
                    return match error {
                        Some(error) => Err(error),
                        None => Ok(ok),
                    };
                }
                Ok(Ok(Message::Reply { id: got, .. })) => {
                    return Err(format!("the renderer answered id {got}, expected {id}"));
                }
                // A notification that arrived mid-call is not this call's answer.
                Ok(Ok(Message::Event(_))) => continue,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err("the renderer exited".into()),
            }
        }
    }

    /// Drains notifications that arrived since the last call.
    pub fn notifications(&self) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            match self.replies.try_recv() {
                Ok(Ok(Message::Event(event))) => events.push(event),
                Ok(Ok(Message::Reply { .. })) => continue,
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => break,
            }
        }
        events
    }

    /// Shows `motion` on `character`, adding it first if it is not there.
    ///
    /// A repeated request for the motion already playing sends nothing, so a
    /// model that asks for `nod` twice does not restart the clip.
    pub fn show(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
        expression: Option<&str>,
    ) -> Result<(), String> {
        let motion = motion.map(|path| path.to_string_lossy().into_owned());
        let config = json!({
            "model": model.to_string_lossy(),
            "motion": motion,
            "playing": true,
            "looping": true,
            "expression": expression,
        });
        let key = motion.clone().unwrap_or_default();
        let op = if self.playing.contains_key(character) {
            if self.playing.get(character) == Some(&key) {
                return Ok(());
            }
            "set"
        } else {
            "add"
        };
        self.call(op, json!({"character": character, "config": config}))?;
        self.playing.insert(character.to_owned(), key);
        Ok(())
    }

    /// Sends the playback amplitude as a one-way notification.
    ///
    /// This is fire-and-forget on purpose: lip sync runs at frame rate, and a
    /// request that waited for a reply would stall the audio callback. A
    /// renderer that has not implemented `lip_sync` ignores the line, which is
    /// why this does not report a failure.
    pub fn lip_sync(&mut self, character: &str, rms: f32) {
        let line = json!({"op": "lip_sync", "character": character, "rms": rms});
        let Ok(mut text) = serde_json::to_string(&line) else {
            return;
        };
        text.push('\n');
        let _ = self.stdin.write_all(text.as_bytes());
    }

    /// Asks the renderer to exit and waits for it.
    pub fn shutdown(mut self) {
        let _ = self.call("quit", json!({}));
        let _ = self.child.wait();
    }
}

impl Drop for Avatar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the renderer's stdout until it closes, forwarding each line.
fn read(stdout: impl std::io::Read, tx: SyncSender<Result<Message, String>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let parsed = serde_json::from_str::<Message>(&line)
                    .map_err(|error| format!("the renderer sent invalid JSON: {error}"));
                if tx.send(parsed).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(format!("the renderer output failed: {error}")));
                break;
            }
        }
    }
}

/// Where the development assets live, matching `scripts/fetch-assets.py`.
pub fn asset_cache() -> Option<PathBuf> {
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    #[cfg(not(windows))]
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(root.join("ene").join("cache"))
}

/// The default model the fetch script installs.
pub fn default_model() -> Option<PathBuf> {
    let path = asset_cache()?.join("models").join("Seed-san.vrm");
    path.is_file().then_some(path)
}

/// The motion file for a preset name, when the fetch script installed one.
///
/// `idle` has no sample, so it resolves to no motion: the renderer then loads
/// the model without a clip instead of failing.
pub fn motion(name: &str) -> Option<PathBuf> {
    let file = match name {
        "nod" => "motions/three-vrm-sample.vrma",
        "tilt_head" => "motions/josie-walk.vrma",
        "stretch" => "motions/quaternius-dance.vrma",
        _ => return None,
    };
    let path = asset_cache()?.join(file);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_and_a_notification_are_distinguished() {
        let reply: Message = serde_json::from_str(r#"{"id":1,"ok":null}"#).unwrap();
        assert!(matches!(reply, Message::Reply { id: 1, .. }));
        let failed: Message =
            serde_json::from_str(r#"{"id":2,"error":"unknown character"}"#).unwrap();
        assert!(matches!(failed, Message::Reply { error: Some(_), .. }));
        let event: Message =
            serde_json::from_str(r#"{"event":"state","character":"ene","status":"ready"}"#)
                .unwrap();
        assert!(matches!(event, Message::Event(_)));
    }

    #[test]
    fn a_preset_without_a_sample_resolves_to_no_motion() {
        // `idle` is a state, not a clip; the renderer must still load the model.
        assert!(motion("idle").is_none());
        assert!(motion("no-such-motion").is_none());
    }
}
