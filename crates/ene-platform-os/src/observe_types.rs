//! Plain data the observer returns.

/// What the sensor can observe right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// The foreground window's title and process name, when readable.
    pub window: Option<Window>,
    /// Seconds since the last input.
    pub idle_seconds: u64,
}

/// A foreground window's title and owning process name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub title: String,
    pub app_id: String,
}
