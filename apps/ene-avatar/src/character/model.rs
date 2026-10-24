//! Avatar data model and configuration validation.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Lifecycle phase and load error, derived from the scene and available outputs.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "error", rename_all = "snake_case")]
pub enum AvatarState {
    #[default]
    Loading,
    Ready,
    Suspended,
    Error(String),
}

impl AvatarState {
    pub(crate) fn json(&self) -> Value {
        let mut value = json!(self);
        if !matches!(self, Self::Error(_)) {
            value["error"] = Value::Null;
        }
        value
    }
}

/// Identity and state root of one avatar. Presence of this component *is* the
/// definition of a character; there is no separate `Character` component.
#[derive(Component, Default)]
#[require(Transform, Visibility, AvatarState, Config)]
pub struct CharacterId(pub String);

/// Settings are accepted atomically by IPC and stored directly on the avatar.
#[derive(Component, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub motion: Option<String>,
    pub playing: bool,
    pub looping: bool,
    pub monitor: Option<String>,
    /// Foot position in logical pixels from the monitor's top-left corner.
    pub position: Option<[f32; 2]>,
    /// 100 draws 1 m as 160 logical pixels.
    pub size_percent: f32,
}

impl Default for Config {
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

impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for path in std::iter::once(&self.model).chain(self.motion.iter()) {
            bevy::asset::AssetPath::try_parse(path).map_err(|error| error.to_string())?;
        }
        let invalid_size = !(50. ..=200.).contains(&self.size_percent)
            || self
                .position
                .is_some_and(|p| p.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.));
        let invalid_path = !self.model.ends_with(".vrm")
            || self
                .motion
                .as_ref()
                .is_some_and(|path| !path.ends_with(".vrma"));
        match (invalid_size, invalid_path) {
            (true, _) => Err("invalid size or position".into()),
            (_, true) => Err("expected .vrm or .vrma path".into()),
            _ => Ok(()),
        }
    }
}

#[derive(Message)]
pub struct Notification(pub Value);
