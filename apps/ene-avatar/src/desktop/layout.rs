//! Durable renderer placement, scoped to the Desktop client that owns it.

use crate::character::Config;
use bevy::prelude::*;
use ene_avatar_ipc::LAYOUT_FILE_ENV;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};

const MAX_LAYOUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Location {
    monitor: Option<String>,
    position: Option<[f32; 2]>,
}

impl From<&Config> for Location {
    fn from(config: &Config) -> Self {
        Self {
            monitor: config.monitor.clone(),
            position: config.position,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    binding: Location,
    local: Location,
}

type Layouts = BTreeMap<String, Saved>;

#[derive(Resource)]
pub(crate) struct Layout {
    layouts: Layouts,
    pending: async_channel::Sender<Layouts>,
    writer: Option<JoinHandle<()>>,
}

impl Layout {
    pub(super) fn from_environment() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os(LAYOUT_FILE_ENV)?);
        if !path.is_absolute() {
            return None;
        }
        let layouts = read(&path).unwrap_or_else(|error| {
            if error.kind() != io::ErrorKind::NotFound {
                warn!(%error, "could not read avatar placement");
            }
            Layouts::new()
        });
        let (pending, saves) = async_channel::bounded(1);
        let writer = thread::Builder::new()
            .name("avatar-layout".into())
            .spawn(move || {
                while let Ok(layouts) = saves.recv_blocking() {
                    if let Err(error) = write(&path, &layouts) {
                        warn!(%error, "could not save avatar placement");
                    }
                }
            })
            .map_err(|error| warn!(%error, "could not start avatar placement writer"))
            .ok()?;
        Some(Self {
            layouts,
            pending,
            writer: Some(writer),
        })
    }

    pub(crate) fn restore(&mut self, character: &str, config: &mut Config) {
        let binding = Location::from(&*config);
        if let Some(saved) = self.layouts.get(character)
            && saved.binding == binding
        {
            config.monitor.clone_from(&saved.local.monitor);
            config.position = saved.local.position;
            return;
        }
        let replaced = self.layouts.insert(
            character.to_owned(),
            Saved {
                local: binding.clone(),
                binding,
            },
        );
        if replaced.is_some() {
            self.queue_save();
        }
    }

    /// An explicit binding change supersedes the saved local placement it changed.
    pub(crate) fn rebind(
        &mut self,
        character: &str,
        update: (Option<String>, Option<[f32; 2]>),
        applied: &Config,
    ) {
        if let Some(saved) = self.layouts.get_mut(character) {
            let before = saved.binding.clone();
            if update.0.is_some() {
                saved.binding.monitor = update.0;
            }
            if update.1.is_some() {
                saved.binding.position = update.1;
            }
            if saved.binding != before {
                saved.local = Location::from(applied);
                self.queue_save();
            }
        }
    }

    pub(crate) fn moved(&mut self, character: &str, config: &Config) {
        if let Some(saved) = self.layouts.get_mut(character) {
            let location = Location::from(config);
            if saved.local != location {
                saved.local = location;
                self.queue_save();
            }
        }
    }

    fn queue_save(&self) {
        // Only the newest full layout needs to be written after an in-flight save.
        if self.pending.force_send(self.layouts.clone()).is_err() {
            warn!("the avatar placement writer closed");
        }
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        self.pending.close();
        if let Some(writer) = self.writer.take()
            && writer.join().is_err()
        {
            warn!("the avatar placement writer panicked");
        }
    }
}

fn read(path: &Path) -> io::Result<Layouts> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_LAYOUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LAYOUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "avatar placement is too large",
        ));
    }
    let mut layouts: Layouts = serde_json::from_slice(&bytes)?;
    layouts.retain(|_, saved| {
        saved.local.position.is_none_or(|position| {
            position
                .iter()
                .all(|value| value.is_finite() && value.abs() <= 1_000_000.)
        })
    });
    Ok(layouts)
}

fn write(path: &Path, layouts: &Layouts) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "avatar placement needs a directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temp.as_file_mut(), layouts)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}
