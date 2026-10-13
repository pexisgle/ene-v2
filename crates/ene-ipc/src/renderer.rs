//! Contracts for one renderer process with one transparent window per avatar.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};

/// Channel halves transferred once from the renderer to its client.
/// Avatar and desktop select the concrete channel types and implement transport.
#[derive(Debug, Serialize, Deserialize)]
pub struct RendererClient<RequestSender, MessageReceiver, StateReceiver> {
    pub requests: RequestSender,
    /// Ordered events and errors.
    pub messages: MessageReceiver,
    /// Latest authoritative state map, independent of message arrival order.
    pub states: StateReceiver,
}

/// Complete latest renderer state. Channel notifications may be coalesced.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RendererState {
    pub config: Option<RendererConfig>,
    pub avatars: HashMap<AvatarId, AvatarState>,
}

/// Desktop-issued identity for one rendered avatar, separate from a companion ID.
/// Desktop allocates increasing IDs and never reuses one within a rendering session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AvatarId(pub u64);

impl fmt::Display for AvatarId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererConfig {
    /// Rendering FPS cap for all windows. 0 disables the app's cap.
    pub fps: u32,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self { fps: 30 }
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
        if self.size_percent < 50 || 200 < self.size_percent {
            return Err("Avatar size must be between 50% and 200%.".into());
        }
        Ok(())
    }
}
/// Position of the avatar window's top-left corner relative to an output's top-left.
/// Coordinates use logical pixels: x increases rightward, y increases downward.
/// Signed coordinates allow a window to extend beyond the output's edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarPlacement {
    pub x: i32,
    pub y: i32,
    /// Target output monitor name; None uses the default output selected by the client.
    pub monitor: Option<String>,
}

// A fixed top-left inset; initial bottom-right placement requires output/window sizes.
impl Default for AvatarPlacement {
    fn default() -> Self {
        Self {
            x: 24,
            y: 24,
            monitor: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
/// One avatar and its dedicated window's initial placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarInstance {
    pub id: AvatarId,
    pub config: AvatarConfig,
    pub placement: AvatarPlacement,
}
impl AvatarInstance {
    pub fn validate(&self) -> Result<(), String> {
        self.config.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RendererRequest {
    /// Initialize the process with a possibly empty list of avatar windows.
    Initialize {
        config: RendererConfig,
        avatars: Vec<AvatarInstance>,
    },
    SetRenderConfig {
        config: RendererConfig,
    },
    /// Create a window. An existing ID must cause an error, not replacement.
    AddAvatar {
        avatar: AvatarInstance,
    },
    /// Release this avatar's window and resources; other avatars remain active.
    RemoveAvatar {
        avatar_id: AvatarId,
    },
    /// Update one avatar without changing its placement or the shared FPS cap.
    SetAvatarConfig {
        avatar_id: AvatarId,
        config: AvatarConfig,
    },
    /// Center the avatar window within its current monitor using logical pixels.
    /// Keep the selected monitor; update the reported placement after moving.
    Recenter {
        avatar_id: AvatarId,
    },
    /// Close all avatar windows and terminate the renderer process.
    Shutdown,
}
impl RendererRequest {
    /// Validate after deserialization, before changing renderer state.
    /// The renderer must also reject existing IDs on add and unknown IDs on
    /// individual operations; those checks require its live avatar registry.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Initialize { avatars, .. } => {
                let mut ids = HashSet::with_capacity(avatars.len());
                for avatar in avatars {
                    avatar.validate()?;
                    if !ids.insert(avatar.id) {
                        return Err(format!("Duplicate avatar ID: {}", avatar.id));
                    }
                }
                Ok(())
            }
            Self::AddAvatar { avatar } => avatar.validate(),
            Self::SetAvatarConfig { config, .. } => config.validate(),
            Self::RemoveAvatar { .. }
            | Self::Recenter { .. }
            | Self::SetRenderConfig { .. }
            | Self::Shutdown => Ok(()),
        }
    }
}

// Many avatar event types may be added as interactions and behaviors are implemented.
// Define new variants when the corresponding features are introduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AvatarEvent {
    MotionFinished,
    /// Open the desktop chat after clicking an avatar without dragging it.
    ShowChat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RendererMessage {
    Event {
        avatar_id: AvatarId,
        event: AvatarEvent,
    },
    /// Emitted after the target window and avatar resources have been released.
    AvatarRemoved { avatar_id: AvatarId },
    Error {
        /// None indicates a process-level error.
        avatar_id: Option<AvatarId>,
        message: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
