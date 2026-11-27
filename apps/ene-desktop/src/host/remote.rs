//! Saved HTTPS preferences and the separate native credential entry they select.

use super::Connect;
use ene_keyring::{Entry, Error as VaultError, Keyring};
use io_lifetimes::AsFilelike as _;
use private_fs::{Durability, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const PREFERENCE: &str = "target.json";
const LEGACY: &str = "remote_host.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteProfile {
    pub endpoint: String,
    #[serde(default)]
    slot: Option<Uuid>,
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
        if let Some(directory) = open_remote_dir(&root)?
            && let Some(profile) = read_profile(&directory)?
        {
            return Ok(Some(profile));
        }
        let mut legacy = match fs::File::open(root.join(LEGACY)) {
            Ok(file) => serde_json::from_reader::<_, Self>(file).map_err(invalid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unavailable(error)),
        };
        legacy.slot = None;
        Ok(Some(legacy))
    }

    pub(super) fn with_token(&self) -> Result<String, Connect> {
        let entry = self
            .entry()?
            .ok_or_else(|| Connect::Invalid("遠隔ホストの資格情報を再登録してください。".into()))?;
        entry.get_password().map_err(unavailable)
    }

    fn entry(&self) -> Result<Option<Entry>, Connect> {
        self.slot
            .map(|slot| slot_entry(&self.endpoint, slot))
            .transpose()
    }

    fn erase(&self) -> Result<(), Connect> {
        if let Some(entry) = self.entry()? {
            match entry.delete_credential() {
                Ok(()) | Err(VaultError::NoEntry) => {}
                Err(error) => return Err(unavailable(error)),
            }
        }
        Ok(())
    }
}

impl RemoteEdit {
    pub(crate) fn persist(self) -> Result<(), Connect> {
        let root = config_root()?;
        fs::create_dir_all(&root).map_err(unavailable)?;
        let directory = PrivateDir::ensure(root.join("remote-host")).map_err(unavailable)?;
        let _lock = lock(&directory)?;
        match self {
            Self::Save { endpoint, token } => {
                save(&directory, read_profile(&directory)?, &endpoint, &token)?
            }
            Self::Clear => {
                match read_profile(&directory) {
                    Ok(Some(current)) => current.erase()?,
                    Ok(None) | Err(Connect::Invalid(_)) => {}
                    Err(error) => return Err(error),
                }
                match directory.remove_file(PREFERENCE, Durability::Full) {
                    Ok(()) => {}
                    Err(private_fs::Error::Io(error))
                        if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(unavailable(error)),
                }
            }
        }
        match fs::remove_file(root.join(LEGACY)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(unavailable(error)),
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

fn read_profile(directory: &PrivateDir) -> Result<Option<RemoteProfile>, Connect> {
    match directory.open_file(PREFERENCE) {
        Ok(file) => serde_json::from_reader(file).map(Some).map_err(invalid),
        Err(private_fs::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
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

fn save(
    directory: &PrivateDir,
    previous: Option<RemoteProfile>,
    endpoint: &str,
    token: &str,
) -> Result<(), Connect> {
    let endpoint = ene_rpc::validate_remote_endpoint(endpoint)
        .map_err(invalid)?
        .to_string();
    if token.is_empty() || token.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(Connect::Invalid("invalid Bearer token".into()));
    }
    let slot = Uuid::new_v4();
    let profile = RemoteProfile {
        endpoint,
        slot: Some(slot),
    };
    let document = serde_json::to_vec(&profile).map_err(invalid)?;
    let entry = slot_entry(&profile.endpoint, slot)?;
    entry.set_password(token).map_err(unavailable)?;
    if let Err(error) = directory.write_atomic_durable(PREFERENCE, &document) {
        // Publication can have taken effect before the final durability barrier.
        if matches!(read_profile(directory), Ok(current) if current.as_ref().and_then(|item| item.slot) != profile.slot)
        {
            let _removed = profile.erase();
        }
        return Err(unavailable(error));
    }
    if let Some(previous) = previous
        && let Err(error) = previous.erase()
    {
        tracing::warn!(%error, "the retired remote credential could not be erased");
    }
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> Connect {
    Connect::Invalid(error.to_string())
}

fn unavailable(error: impl std::fmt::Display) -> Connect {
    Connect::Unavailable(error.to_string())
}
