//! Plain data the observer returns.

/// What the sensor can observe right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub window: ObservedWindow,
    /// Seconds since the last input, when activity can be observed.
    pub idle_seconds: Option<u64>,
}

/// The difference between an empty foreground and an unreadable one matters
/// to rules that act on focus transitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ObservedWindow {
    #[default]
    Unavailable,
    Empty,
    Window(Window),
}

/// A foreground window's title and owning process name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub title: String,
    pub app_id: String,
}
