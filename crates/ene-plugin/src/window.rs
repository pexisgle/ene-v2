//! Desktop window observations. Native window handles never cross this boundary.
use serde::{Deserialize, Serialize};

/// Opaque identity, valid only in this device connection generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowId {
    pub device_id: String,
    pub generation: u64,
    pub id: String,
}

/// Physical pixels in the desktop coordinate space; origins may be negative.
/// Consumers must convert to the destination monitor's logical coordinates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowInfo {
    pub id: WindowId,
    /// Platform application identifier, supplied by the desktop, not a window title.
    pub application: String,
    /// Omitted unless title access was requested and granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// None when the platform cannot supply geometry (including after closure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<WindowBounds>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowEventKind {
    Opened,
    Closed,
    Focused,
    Unfocused,
    Moved,
    Resized,
    Minimized,
    Restored,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowEvent {
    /// Monotonically increasing within the device connection generation.
    pub sequence: u64,
    pub kind: WindowEventKind,
    pub window: WindowInfo,
}

/// A request for observation, never a permission grant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSubscription {
    /// Exact platform identifiers. Empty means no applications, never all.
    pub applications: Vec<String>,
    pub events: Vec<WindowEventKind>,
    #[serde(default)]
    pub include_title: bool,
}

/// Report actual platform support; an empty list means unavailable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCapabilities {
    pub events: Vec<WindowEventKind>,
    pub titles: bool,
    pub bounds: bool,
}
