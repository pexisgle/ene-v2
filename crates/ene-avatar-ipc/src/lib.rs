//! Shared JSON Lines IPC contract between `ene-desktop` and `ene-avatar`.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
use serde::{Deserialize, Serialize};

/// Maximum JSON line length accepted on stdin by `ene-avatar`.
pub const MAX_LINE: usize = 65_536;

/// Optional local layout file for the device that owns the renderer process.
pub const LAYOUT_FILE_ENV: &str = "ENE_AVATAR_LAYOUT_FILE";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CharacterConfig {
    pub model: String,
    pub motion: Option<String>,
    pub playing: bool,
    pub looping: bool,
    pub monitor: Option<String>,
    /// Foot position in logical pixels from the monitor's top-left corner.
    pub position: Option<[f32; 2]>,
    /// 100 draws 1 m as 320 logical pixels.
    pub size_percent: f32,
}

impl Default for CharacterConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            motion: None,
            playing: true,
            looping: true,
            monitor: None,
            position: None,
            size_percent: 100.,
        }
    }
}

impl CharacterConfig {
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    pub fn with_scale(&self, size_percent: f32) -> Self {
        let mut next = self.clone();
        next.size_percent = size_percent.clamp(50., 200.);
        next
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Request {
    /// Absent on one-way notifications such as `lip_sync`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("{0}")]
    Serialize(#[from] serde_json::Error),
    #[error("the request exceeds the renderer's 64 KiB line limit")]
    LineTooLong,
}

impl Request {
    pub fn encode_line(&self) -> Result<String, EncodeError> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        if line.len() > MAX_LINE {
            return Err(EncodeError::LineTooLong);
        }
        Ok(line)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Add {
        character: String,
        config: CharacterConfig,
    },
    Set {
        character: String,
        config: CharacterConfig,
    },
    /// Changes configuration while keeping any omitted monitor or position at
    /// its current renderer value. `Set` replaces the entire configuration.
    Update {
        character: String,
        config: CharacterConfig,
    },
    Motion {
        character: String,
        motion: Option<String>,
        looping: bool,
    },
    Get {},
    Remove {
        character: String,
    },
    Center {
        character: String,
        monitor: Option<String>,
    },
    Fps {
        value: u32,
    },
    Expression {
        character: String,
        /// `null` clears the face back to neutral.
        name: Option<String>,
    },
    /// One-way: playback amplitude, sent without an id and never answered.
    LipSync {
        character: String,
        rms: f32,
    },
    /// Sets or clears direct gaze angles in degrees (`None` returns gaze to animation/neutral).
    LookAt {
        character: String,
        #[serde(default)]
        yaw_deg: Option<f32>,
        #[serde(default)]
        pitch_deg: Option<f32>,
    },
    /// Moves the character's foot position on an optional target monitor (logical pixels).
    Place {
        character: String,
        #[serde(default)]
        monitor: Option<String>,
        position: [f32; 2],
    },
    Quit {},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarStatus {
    Loading,
    Ready,
    Suspended,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AvatarEvent {
    State {
        character: String,
        status: AvatarStatus,
        #[serde(default)]
        error: Option<String>,
    },
    Finished {
        character: String,
        /// The motion request that began this playback, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
    },
    Click {
        character: String,
    },
}

impl AvatarEvent {
    pub fn character(&self) -> &str {
        match self {
            Self::State { character, .. }
            | Self::Finished { character, .. }
            | Self::Click { character } => character,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AvatarMessage {
    ErrorReply {
        id: u64,
        error: String,
    },
    Reply {
        id: u64,
        #[serde(default)]
        ok: Option<RendererSnapshot>,
    },
    Event(AvatarEvent),
    Fatal {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CharacterSnapshot {
    pub character: String,
    pub status: AvatarStatus,
    #[serde(default)]
    pub error: Option<String>,
    pub config: CharacterConfig,
    #[serde(default)]
    pub expression: Option<String>,
    #[serde(default)]
    pub lip: f32,
    #[serde(default)]
    pub lip_level: f32,
    #[serde(default)]
    pub expression_weight: f32,
    #[serde(default)]
    pub animation_times: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct MonitorSnapshot {
    pub name: String,
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub units_per_logical_pixel: f32,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RendererSnapshot {
    #[serde(default)]
    pub characters: Vec<CharacterSnapshot>,
    #[serde(default)]
    pub monitors: Vec<MonitorSnapshot>,
    #[serde(default)]
    pub fps: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_commands_and_messages() {
        let req = Request {
            id: Some(7),
            command: Command::Motion {
                character: "ene".into(),
                motion: Some("/motions/nod.vrma".into()),
                looping: false,
            },
        };
        let line = req.encode_line().unwrap();
        let decoded: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, req);

        let reply: AvatarMessage =
            serde_json::from_str(r#"{"type":"reply","id":1,"ok":null}"#).unwrap();
        assert!(matches!(reply, AvatarMessage::Reply { id: 1, .. }));
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            r#"{"type":"reply","id":1,"ok":null}"#
        );
        let fatal = r#"{"type":"fatal","error":"missing compositor protocol"}"#;
        let message: AvatarMessage = serde_json::from_str(fatal).unwrap();
        assert!(matches!(message, AvatarMessage::Fatal { .. }));
        assert_eq!(serde_json::to_string(&message).unwrap(), fatal);

        let click: AvatarMessage =
            serde_json::from_str(r#"{"type":"event","event":"click","character":"ene"}"#).unwrap();
        assert_eq!(
            click,
            AvatarMessage::Event(AvatarEvent::Click {
                character: "ene".into()
            })
        );
    }
}
