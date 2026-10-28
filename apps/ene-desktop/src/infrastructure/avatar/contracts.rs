//! Desktop-side renderer data. The renderer takes and reports the same shapes
//! as JSON Lines, so these types stay local to the client.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Identity of one rendered character within the current desktop session.
/// Desktop allocates increasing IDs and never reuses one within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AvatarId(pub u64);

impl fmt::Display for AvatarId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarConfig {
    pub model: String,
    pub motion: Option<String>,
    /// Play the selected motion when true; pause it when false.
    pub motion_playing: bool,
    /// Repeat the selected motion after it finishes.
    pub motion_loop: bool,
    pub size_percent: u32,
}

impl Default for AvatarConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            motion: None,
            motion_playing: true,
            motion_loop: true,
            size_percent: 100,
        }
    }
}

impl AvatarConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(50..=200).contains(&self.size_percent) {
            return Err("Avatar size must be between 50% and 200%.".into());
        }
        Ok(())
    }
}

/// Foot position of one character relative to an output's top-left corner.
/// Coordinates use logical pixels: x increases rightward, y increases downward.
/// A missing position leaves the renderer's default placement in effect until
/// the user drags or recenters the character.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarPlacement {
    pub position: Option<[i32; 2]>,
    /// Target output monitor name; None uses the first output.
    pub monitor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AvatarState {
    pub placement: AvatarPlacement,
    pub loading: bool,
    pub model: Option<String>,
    pub motion: MotionState,
    pub error: Option<String>,
}

impl Default for AvatarState {
    fn default() -> Self {
        Self {
            placement: AvatarPlacement::default(),
            loading: true,
            model: None,
            motion: MotionState::None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarEvent {
    MotionFinished,
    /// Open the desktop chat after clicking an avatar without dragging it.
    ShowChat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotionState {
    None,
    Loading,
    Playing,
    Paused,
    Finished,
    /// The selected motion failed to load or play; no motion remains playing.
    Failed {
        message: String,
    },
}
