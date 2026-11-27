//! Desktop observation and local `eye.watch` rules.

use std::{
    io::{self, Write},
    path::PathBuf,
};

mod rules;
#[cfg(windows)]
pub(crate) mod screen;
#[cfg(test)]
mod tests;

use ene_platform_os::Observation;
pub use rules::{IDLE_THRESHOLD, NotifyOn, Report, Sensor, Watch, Window};
#[cfg(windows)]
pub(crate) use screen::{capture, unlocked};

fn preference_path(name: &str) -> io::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ene")
        .ok_or_else(|| io::Error::other("設定ディレクトリを取得できません"))?;
    Ok(dirs.config_local_dir().join(name))
}

pub(crate) fn enabled() -> bool {
    preference_enabled("desktop-eyes")
}

pub(crate) fn preference_enabled(name: &str) -> bool {
    preference_path(name)
        .and_then(std::fs::read)
        .is_ok_and(|value| value == b"on")
}

pub(crate) fn save_enabled(enabled: bool) -> io::Result<()> {
    save_preference("desktop-eyes", enabled)
}

pub(crate) fn save_preference(name: &str, enabled: bool) -> io::Result<()> {
    let path = preference_path(name)?;
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("a preference path has no directory"))?;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(if enabled { b"on" } else { b"off" })?;
    file.persist(path)?;
    Ok(())
}

/// Whether this platform can report the foreground window at all.
///
/// A platform without the hook declares no `eye.watch`, so the host never
/// publishes a tool this client cannot honour.
pub fn supported() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        ene_platform_os::observation_supported()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Samples the current foreground window and idle time.
#[cfg(any(windows, target_os = "linux"))]
pub fn observe() -> Observation {
    ene_platform_os::observe()
}

/// A platform without a window hook reports no window, and says so by
/// returning `None` rather than inventing one.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn observe() -> Observation {
    Observation::default()
}
