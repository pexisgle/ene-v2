//! Contracts for one renderer process with one transparent window per avatar.

use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};

/// Session transferred once over the child process's stdin/stdout connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct RendererSession {
    pub renderer: RendererClient,
    pub messages: remoc::rch::mpsc::Receiver<RendererMessage>,
    /// Latest authoritative state, independent of RPC and event arrival order.
    pub states: remoc::rch::watch::Receiver<RendererState>,
}

/// Operation rejection or failure of the remote call itself.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum RendererError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Call(#[from] remoc::rtc::CallError),
}

/// Renderer operations. The macro generates the client and server.
/// Success acknowledges registration/removal or queueing a window command;
/// model loading and actual window state are reported through the session's watch.
#[remoc::rtc::remote(server(Value))]
pub trait Renderer {
    /// Initialize once; validate all avatars before registering any windows.
    async fn initialize(
        &mut self,
        config: RendererConfig,
        avatars: Vec<AvatarInstance>,
    ) -> Result<(), RendererError>;
    /// Update the shared FPS cap; per-window delivery failures are asynchronous errors.
    async fn set_render_config(&mut self, config: RendererConfig) -> Result<(), RendererError>;
    /// Register a new ID and start its window; reject an existing ID.
    async fn add_avatar(&mut self, avatar: AvatarInstance) -> Result<(), RendererError>;
    /// Returns after this avatar's window and resources have been released.
    async fn remove_avatar(&mut self, avatar_id: AvatarId) -> Result<(), RendererError>;
    /// Queue settings without changing placement or the shared FPS cap.
    async fn set_avatar_config(
        &mut self,
        avatar_id: AvatarId,
        config: AvatarConfig,
    ) -> Result<(), RendererError>;
    /// Queue centering on the current monitor; report the resulting placement in watch.
    async fn recenter(&mut self, avatar_id: AvatarId) -> Result<(), RendererError>;
    /// Release all windows and stop the server; use the child process handle to await exit.
    async fn shutdown(self) -> Result<(), RendererError>;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
