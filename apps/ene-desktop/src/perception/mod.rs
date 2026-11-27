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

pub(crate) struct Observer {
    #[cfg(target_os = "linux")]
    source: ene_platform_os::Observer,
}

impl Observer {
    pub(crate) fn open() -> Option<Self> {
        #[cfg(windows)]
        {
            Some(Self {})
        }
        #[cfg(target_os = "linux")]
        {
            ene_platform_os::Observer::open().map(|source| Self { source })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            None
        }
    }

    pub(crate) fn observe(&mut self) -> Observation {
        #[cfg(windows)]
        {
            ene_platform_os::observe()
        }
        #[cfg(target_os = "linux")]
        {
            self.source.observe()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Observation::default()
        }
    }
}
