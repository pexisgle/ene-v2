//! Saved HTTPS preferences and the separate native credential entry they select.

use super::Connect;
use ene_keyring::{Entry, Error as VaultError, Keyring};
use io_lifetimes::AsFilelike as _;
use private_fs::{Durability, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const PREFERENCE: &str = "target.json";
const PENDING: &str = "pending.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RemoteProfile {
    pub endpoint: String,
    slot: Uuid,
}

#[derive(Deserialize, Serialize)]
struct Publication {
    previous: Option<RemoteProfile>,
    next: Option<RemoteProfile>,
}

pub(crate) enum RemoteEdit {
    Save { endpoint: String, token: String },
    Clear,
}

impl RemoteProfile {
    pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), Connect> {
        ene_rpc::validate_remote_endpoint(endpoint)
            .map(|_| ())
            .map_err(invalid)
    }

    pub fn read() -> Result<Option<Self>, Connect> {
        let root = config_root()?;
        match open_remote_dir(&root)? {
            Some(directory) => read_json(&directory, PREFERENCE),
            None => Ok(None),
        }
    }

    /// Retries a previously interrupted edit on a background thread.
    pub fn recover() -> Result<(), Connect> {
        let root = config_root()?;
        if let Some(directory) = open_remote_dir(&root)?
            && read_json::<Publication>(&directory, PENDING)?.is_some()
        {
            let _lock = lock(&directory)?;
            finish_publication(&directory)?;
        }
        Ok(())
    }

    pub(super) fn with_token(&self) -> Result<String, Connect> {
        slot_entry(&self.endpoint, self.slot)?
            .get_password()
            .map_err(unavailable)
    }

    fn erase(&self) -> Result<(), Connect> {
        match slot_entry(&self.endpoint, self.slot)?.delete_credential() {
            Ok(()) | Err(VaultError::NoEntry) => Ok(()),
            Err(error) => Err(unavailable(error)),
        }
    }
}

impl RemoteEdit {
    pub(crate) fn persist(self) -> Result<(), Connect> {
        let root = config_root()?;
        fs::create_dir_all(&root).map_err(unavailable)?;
        let directory = PrivateDir::ensure(root.join("remote-host")).map_err(unavailable)?;
        let _lock = lock(&directory)?;
        finish_publication(&directory)?;
        match self {
            Self::Save { endpoint, token } => {
                save(&directory, &endpoint, &token)?;
                finish_publication(&directory)
            }
            Self::Clear => {
                let previous = match read_json(&directory, PREFERENCE) {
                    Ok(previous) => previous,
                    Err(Connect::Invalid(_)) => None,
                    Err(error) => return Err(error),
                };
                if previous.is_some() {
                    write_json(
                        &directory,
                        PENDING,
                        &Publication {
                            previous,
                            next: None,
                        },
                    )?;
                }
                remove_file(&directory, PREFERENCE)?;
                finish_publication(&directory)
            }
        }
    }
}

fn config_root() -> Result<PathBuf, Connect> {
    directories::ProjectDirs::from("", "", "ene")
        .map(|dirs| dirs.config_dir().to_owned())
        .ok_or(Connect::MissingConfigDir)
}

fn open_remote_dir(root: &Path) -> Result<Option<PrivateDir>, Connect> {
    match PrivateDir::open(root.join("remote-host")) {
        Ok(directory) => Ok(Some(directory)),
        Err(private_fs::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(unavailable(error)),
    }
}

fn read_json<T: DeserializeOwned>(
    directory: &PrivateDir,
    name: &str,
) -> Result<Option<T>, Connect> {
    match directory.open_file(name) {
        Ok(file) => serde_json::from_reader(file).map(Some).map_err(invalid),
        Err(private_fs::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(unavailable(error)),
    }
}

fn write_json(directory: &PrivateDir, name: &str, value: &impl Serialize) -> Result<(), Connect> {
    directory
        .write_atomic_durable(name, &serde_json::to_vec(value).map_err(invalid)?)
        .map_err(unavailable)
}

fn remove_file(directory: &PrivateDir, name: &str) -> Result<(), Connect> {
    match directory.remove_file(name, Durability::Full) {
        Ok(()) => Ok(()),
        Err(private_fs::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(unavailable(error)),
    }
}

fn lock(directory: &PrivateDir) -> Result<PrivateFile, Connect> {
    let options = OpenOptions {
        access: FileAccess::ReadWrite,
        creation: FileCreation::OpenOrCreate,
        ..Default::default()
    };
    let file = directory
        .open_file_with("preference.lock", options)
        .map_err(unavailable)?;
    file.as_filelike_view::<fs::File>()
        .try_lock()
        .map_err(unavailable)?;
    Ok(file)
}

fn slot_entry(endpoint: &str, slot: Uuid) -> Result<Entry, Connect> {
    static KEYRING: Keyring = Keyring::new();
    KEYRING
        .entry("ene-desktop-remote", &format!("{slot}:{endpoint}"))
        .map_err(unavailable)
}

fn save(directory: &PrivateDir, endpoint: &str, token: &str) -> Result<(), Connect> {
    let endpoint = ene_rpc::validate_remote_endpoint(endpoint)
        .map_err(invalid)?
        .to_string();
    if token.is_empty() || token.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(Connect::Invalid("invalid Bearer token".into()));
    }
    let slot = Uuid::new_v4();
    let profile = RemoteProfile { endpoint, slot };
    write_json(
        directory,
        PENDING,
        &Publication {
            previous: read_json(directory, PREFERENCE)?,
            next: Some(profile.clone()),
        },
    )?;
    let entry = slot_entry(&profile.endpoint, slot)?;
    entry.set_password(token).map_err(unavailable)?;
    write_json(directory, PREFERENCE, &profile)
}

fn finish_publication(directory: &PrivateDir) -> Result<(), Connect> {
    let Some(edit) = read_json::<Publication>(directory, PENDING)? else {
        return Ok(());
    };
    let active = read_json(directory, PREFERENCE)?;
    // The active pointer can already be visible after a failed durability barrier.
    directory.sync_all().map_err(unavailable)?;
    let retired = if active == edit.next {
        edit.previous
    } else if active == edit.previous {
        edit.next
    } else {
        return Err(Connect::Unavailable(
            "the saved remote target changed during recovery".into(),
        ));
    };
    if let Some(retired) = retired {
        retired.erase()?;
    }
    remove_file(directory, PENDING)
}

fn invalid(error: impl std::fmt::Display) -> Connect {
    Connect::Invalid(error.to_string())
}

fn unavailable(error: impl std::fmt::Display) -> Connect {
    Connect::Unavailable(error.to_string())
}
