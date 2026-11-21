//! Desktop observation and local `eye.watch` rules.

use std::{
    io::{self, Write},
    path::PathBuf,
};

#[cfg(target_os = "linux")]
mod platform_linux;
#[cfg(windows)]
mod platform_windows;
mod rules;
#[cfg(windows)]
pub(crate) mod screen;
#[cfg(test)]
mod tests;

pub use rules::{
    IDLE_THRESHOLD, NotifyOn, Observation, Report, Sensor, Watch, WatchParseError, Window,
};
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
        platform_linux::supported()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Samples the OS once.
#[cfg(windows)]
pub fn observe() -> Observation {
    platform_windows::observe()
}

/// Samples the OS once on Linux (Wayland `wlr-foreign-toplevel-management`
/// or X11 `_NET_ACTIVE_WINDOW`), falling back safely when unavailable.
#[cfg(target_os = "linux")]
pub fn observe() -> Observation {
    platform_linux::observe()
}

/// A platform without a window hook reports no window, and says so by
/// returning `None` rather than inventing one.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn observe() -> Observation {
    Observation {
        window: None,
        idle_seconds: 0,
    }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_wm_class(bytes: &[u8]) -> String {
    let mut parts = bytes
        .split(|&byte| byte == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    if !class.is_empty() { class } else { instance }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn wayland_state_is_activated(bytes: &[u8]) -> bool {
    const ACTIVATED: u32 = 2;
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .any(|chunk| u32::from_ne_bytes(*chunk) == ACTIVATED)
}
