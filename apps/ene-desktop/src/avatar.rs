//! The avatar renderer as a child process.
//!
//! `apps/ene-avatar` speaks JSON Lines on stdin and stdout (`src/control.rs`).
//! This module owns that process: it frames requests, drains replies and
//! notifications, and turns a `body.motion` operation into an `add` or a `set`.
//!
//! `set` replaces the whole character configuration, so this module keeps the
//! configuration it last sent and resends it with the one field that changed.
//! Sending only a motion would silently reset the monitor, position and size.
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

/// The character configuration this client last sent.
///
/// This mirrors `Character::Config` in `apps/ene-avatar/src/character.rs`, which
/// is a binary crate that a GUI must not link. Every field is carried so a
/// motion change cannot reset the placement.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    model: String,
    motion: Option<String>,
    monitor: Option<String>,
    position: Option<[f32; 2]>,
    size_percent: f32,
}

impl Shown {
    fn new(model: &Path) -> Self {
        Self {
            model: model.to_string_lossy().into_owned(),
            motion: None,
            monitor: None,
            position: None,
            size_percent: 100.,
        }
    }

    fn config(&self) -> Value {
        json!({
            "model": self.model,
            "motion": self.motion,
            "playing": true,
            "looping": true,
            "monitor": self.monitor,
            "position": self.position,
            "size_percent": self.size_percent,
        })
    }

    fn with_scale(&self, size_percent: f32) -> Self {
        let mut next = self.clone();
        next.size_percent = size_percent.clamp(50., 200.);
        next
    }
}

/// The request body for a face change. `None` is the renderer's "clear it".
fn expression_request(character: &str, name: Option<&str>) -> Value {
    json!({"character": character, "name": name})
}

/// One mouth amplitude as the renderer's one-way line.
///
/// No `id`: the renderer answers nothing for this op, which is what lets it be
/// sent at the audio rate without a reply queue.
fn lip_sync_line(character: &str, rms: f32) -> Result<String, String> {
    let mut line = serde_json::to_string(&json!({
        "op": "lip_sync",
        "character": character,
        "rms": rms.clamp(0., 1.),
    }))
    .map_err(|error| error.to_string())?;
    line.push('\n');
    Ok(line)
}
/// The renderer process and the channels to its stdio.
pub struct Avatar {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Result<Message, String>>,
    next_id: u64,
    /// What each character was last configured to, so a repeated request is a
    /// no-op and a partial change stays partial.
    shown: HashMap<String, Shown>,
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
            shown: HashMap::new(),
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

    /// Drains notifications that arrived since the last call, so the renderer's
    /// output queue never fills.
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
    /// A repeated request for the motion already showing sends nothing, so a
    /// model that asks for `nod` twice does not restart the clip.
    pub fn show(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
    ) -> Result<(), String> {
        let motion = motion.map(|path| path.to_string_lossy().into_owned());
        let next = match self.shown.get(character) {
            Some(current) => {
                if current.motion == motion && current.model == model.to_string_lossy() {
                    return Ok(());
                }
                let mut next = current.clone();
                next.motion = motion;
                next
            }
            None => {
                let mut next = Shown::new(model);
                next.motion = motion;
                next
            }
        };
        let op = if self.shown.contains_key(character) {
            "set"
        } else {
            "add"
        };
        self.call(op, json!({"character": character, "config": next.config()}))?;
        self.shown.insert(character.to_owned(), next);
        Ok(())
    }

    /// Sets the character's face, or clears it with `None`.
    ///
    /// Unlike a motion this does not change the configuration the renderer was
    /// given, so it survives a later motion change.
    pub fn expression(&mut self, character: &str, name: Option<&str>) -> Result<(), String> {
        self.call("expression", expression_request(character, name))?;
        Ok(())
    }

    /// Updates the character's display scale while keeping motion and placement.
    pub fn set_scale(
        &mut self,
        character: &str,
        model: &Path,
        size_percent: f32,
    ) -> Result<(), String> {
        let clamped = size_percent.clamp(50., 200.);
        let (op, next) = match self.shown.get(character) {
            Some(current) => ("set", current.with_scale(clamped)),
            None => ("add", Shown::new(model).with_scale(clamped)),
        };
        self.call(op, json!({"character": character, "config": next.config()}))?;
        self.shown.insert(character.to_owned(), next);
        Ok(())
    }

    /// Centers the character on the primary monitor.
    pub fn center(&mut self, character: &str) -> Result<(), String> {
        self.call("center", json!({"character": character}))?;
        Ok(())
    }

    /// Sends one mouth amplitude, with no id and no reply.
    ///
    /// This runs at the audio rate, so it must never wait for the renderer: a
    /// fire-and-forget line is the whole point of the `lip_sync` op.
    pub fn lip_sync(&mut self, character: &str, rms: f32) -> Result<(), String> {
        let line = lip_sync_line(character, rms)?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("the renderer input closed: {error}"))
    }
    /// What the renderer is showing, for the device footer.
    pub fn describe(&self, character: &str) -> Option<String> {
        self.shown.get(character).map(|shown| {
            let model = shown
                .model
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&shown.model)
                .to_owned();
            match shown
                .motion
                .as_ref()
                .and_then(|motion| motion.rsplit(['\\', '/']).next())
            {
                Some(motion) => format!("{model} ({motion})"),
                None => model,
            }
        })
    }

    /// Waits for the renderer to say whether the character actually loaded.
    ///
    /// `add` and `set` answer as soon as they are accepted; the model and motion
    /// finish loading later and the renderer publishes `state` events. Reporting
    /// the reply alone would call a failed load a success, so the operation waits
    /// for the terminal state. A slow load that is still in progress when the
    /// wait ends is not a failure, so it is reported as accepted.
    pub fn await_state(
        &mut self,
        character: &str,
        wait: std::time::Duration,
    ) -> Result<(), String> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match self.replies.recv_timeout(remaining) {
                Ok(Ok(Message::Event(event))) => {
                    if event.get("event").and_then(Value::as_str) != Some("state")
                        || event.get("character").and_then(Value::as_str) != Some(character)
                    {
                        continue;
                    }
                    match event.get("status").and_then(Value::as_str) {
                        Some("ready") | Some("suspended") => return Ok(()),
                        Some("error") => {
                            let reason = event
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("the renderer did not load the model");
                            return Err(reason.to_owned());
                        }
                        _ => continue,
                    }
                }
                // A stray reply is not ours to interpret here.
                Ok(Ok(Message::Reply { .. })) => continue,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Ok(()),
            }
        }
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
    // The script writes `%LOCALAPPDATA%\ene\cache` on Windows and
    // `$XDG_CACHE_HOME/ene` (no extra level) elsewhere.
    #[cfg(windows)]
    {
        let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        Some(root.join("ene").join("cache"))
    }
    #[cfg(not(windows))]
    {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
        Some(root.join("ene"))
    }
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

    /// The renderer's `set` replaces the whole configuration, so changing the
    /// motion must carry the placement that was already there.
    #[test]
    fn changing_a_motion_keeps_the_placement_fields() {
        let mut shown = Shown::new(Path::new("/models/ene.vrm"));
        shown.monitor = Some("eDP-1".into());
        shown.position = Some([100., 200.]);
        shown.size_percent = 140.;
        let mut next = shown.clone();
        next.motion = Some("/motions/nod.vrma".into());
        let config = next.config();
        assert_eq!(config["monitor"], "eDP-1");
        assert_eq!(config["position"][1], 200.0);
        assert_eq!(config["size_percent"], 140.0);
        assert_eq!(config["motion"], "/motions/nod.vrma");
    }
    #[test]
    fn a_face_request_can_be_set_and_cleared() {
        let set = expression_request("ene", Some("happy"));
        assert_eq!(set["character"], "ene");
        assert_eq!(set["name"], "happy");
        let clear = expression_request("ene", None);
        assert!(clear["name"].is_null());
    }

    #[test]
    fn the_lip_line_is_one_way_and_clamped() {
        let line = lip_sync_line("ene", 2.5).unwrap();
        assert!(line.ends_with('\n'));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["op"], "lip_sync");
        assert_eq!(parsed["character"], "ene");
        assert_eq!(parsed["rms"], 1.0);
        assert!(parsed.get("id").is_none());

        let parsed: Value =
            serde_json::from_str(lip_sync_line("ene", -1.0).unwrap().trim_end()).unwrap();
        assert_eq!(parsed["rms"], 0.0);
    }

    #[test]
    fn scaling_keeps_motion_and_placement_fields() {
        let mut shown = Shown::new(Path::new("/models/ene.vrm"));
        shown.motion = Some("/motions/nod.vrma".into());
        shown.monitor = Some("eDP-1".into());
        shown.position = Some([120., 240.]);
        let scaled = shown.with_scale(250.0);
        assert_eq!(scaled.size_percent, 200.0);
        assert_eq!(scaled.motion.as_deref(), Some("/motions/nod.vrma"));
        assert_eq!(scaled.monitor.as_deref(), Some("eDP-1"));
        assert_eq!(scaled.position, Some([120., 240.]));

        let min_scaled = shown.with_scale(10.0);
        assert_eq!(min_scaled.size_percent, 50.0);
    }
}
