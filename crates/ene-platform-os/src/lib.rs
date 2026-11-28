mod observation;
pub use observation::{Observation, ObservedWindow, Window};

#[cfg(windows)]
#[path = "observe_windows.rs"]
mod sensor;
#[cfg(windows)]
pub use sensor::observe;

#[cfg(windows)]
pub mod windows;
