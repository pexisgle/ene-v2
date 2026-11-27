//! Credential storage and lookup for provider connections.
//!
//! A registered secret lives in the OS keyring, or only in memory when the host
//! was told to keep sessions credential-free (`ENE_HOST_SESSION_CREDENTIALS`).
//! An unreadable registered credential fails closed. An injected environment
//! credential is available independently for an unregistered provider endpoint.
use crate::host::db::{Result, Store};
use crate::host::error::HostError;
use crate::host::settings::{
    Connection, ConnectionConfig, CredentialState, environment_secret, validate_settings_revision,
};
#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::Store as PlatformVault;
use ene_store::credentials::VaultSlot;
use keyring_core::api::CredentialStoreApi as _;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path, sync::Arc};
#[cfg(windows)]
use windows_native_keyring_store::Store as PlatformVault;
#[cfg(target_os = "linux")]
use zbus_secret_service_keyring_store::Store as PlatformVault;

const KEYRING_SERVICE: &str = "ene-host";

#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Credentials {
    memory: Arc<Mutex<CredentialMemory>>,
    vault: Option<Arc<ManagedVault>>,
    session_only: bool,
    pub(crate) approval_key: Option<[u8; 32]>,
    /// Test-only: makes the credential store unreadable, so a test can check
    /// that the failure is reported instead of becoming "no credential".
    #[cfg(test)]
    unreadable: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct CredentialMemory {
    generation: u64,
    generations: HashMap<String, u64>,
    session_slots: HashMap<uuid::Uuid, String>,
    #[cfg(test)]
    injected: HashMap<String, String>,
}

struct ManagedVault {
    store: Store,
    namespace: String,
    mutations: Mutex<()>,
    platform: OnceCell<Arc<PlatformVault>>,
}

impl CredentialMemory {
    fn record_change(&mut self, connection: &str) {
        self.generation += 1;
        self.generations
            .insert(connection.to_owned(), self.generation);
    }
}

impl Credentials {
    #[cfg(test)]
    pub(crate) fn new(session_only: bool) -> Self {
        Self {
            session_only,
            ..Self::default()
        }
    }

    pub(crate) fn managed(
        store: &Store,
        state: &Path,
        session_only: bool,
    ) -> ene_store::Result<Self> {
        let path = state.canonicalize().map_err(|error| {
            ene_store::StoreError::Storage(format!("credential state directory: {error}"))
        })?;
        #[cfg(unix)]
        let digest = {
            use std::os::unix::ffi::OsStrExt as _;
            Sha256::digest(path.as_os_str().as_bytes())
        };
        #[cfg(windows)]
        let digest = {
            use std::os::windows::ffi::OsStrExt as _;
            let bytes: Vec<_> = path
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect();
            Sha256::digest(bytes)
        };
        store.credential_repo().expire_sessions()?;
        Ok(Self {
            vault: Some(Arc::new(ManagedVault {
                store: store.clone(),
                namespace: ene_policy::hex_digest(&digest),
                mutations: Mutex::new(()),
                platform: OnceCell::new(),
            })),
            session_only,
            ..Self::default()
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        self.memory.lock().generation
    }

    pub(crate) fn generation_of(&self, connection: &str) -> u64 {
        self.memory
            .lock()
            .generations
            .get(connection)
            .copied()
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn remember(&self, connection: &str, secret: String) {
        let mut memory = self.memory.lock();
        memory.injected.insert(connection.to_owned(), secret);
        memory.record_change(connection);
    }

    pub(crate) fn forget(&self, connection: &str) {
        let mut memory = self.memory.lock();
        #[cfg(test)]
        memory.injected.remove(connection);
        memory.record_change(connection);
    }

    /// The registered secret, `None` when none is registered. An unreadable
    /// credential store is an error, not "no credential": a request must not
    /// go out unauthenticated because the store could not be read.
    pub(crate) fn secret(&self, connection: &str) -> Result<Option<String>> {
        self.read_secret(connection, None)
    }

    fn read_secret(
        &self,
        connection: &str,
        receiver: Option<&ConnectionConfig>,
    ) -> Result<Option<String>> {
        #[cfg(test)]
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(HostError::Unavailable(
                "credential store: unreadable".into(),
            ));
        }
        let Some(vault) = &self.vault else {
            #[cfg(test)]
            return Ok(self.memory.lock().injected.get(connection).cloned());
            #[cfg(not(test))]
            return Ok(None);
        };
        let Some(binding) = vault.store.credential_repo().binding(connection)? else {
            return Ok(None);
        };
        if receiver.is_some_and(|receiver| !binding.receiver.same_credential_receiver(receiver)) {
            return Err(HostError::Conflict(
                "the credential receiver changed".into(),
            ));
        }
        if binding.slot.session_only {
            return Ok(self
                .memory
                .lock()
                .session_slots
                .get(&binding.slot.id)
                .cloned());
        }
        if self.session_only {
            return Ok(None);
        }
        match vault.entry(binding.slot)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(vault_error(&error)),
        }
    }

    #[cfg(test)]
    pub(crate) fn make_unreadable(&self) {
        self.unreadable
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn session_only(&self) -> bool {
        self.session_only
    }

    pub(crate) fn recover_detached(&self) {
        if let Some(vault) = &self.vault
            && let Err(error) = self.erase_detached(vault)
        {
            tracing::warn!(%error, "credential erasure is pending");
        }
    }

    fn erase_detached(&self, vault: &ManagedVault) -> Result<()> {
        let _guard = vault.mutations.lock();
        let repo = vault.store.credential_repo();
        for slot in repo.detached()? {
            if slot.session_only {
                self.memory.lock().session_slots.remove(&slot.id);
            } else if self.session_only {
                continue;
            } else {
                match vault.entry(slot)?.delete_credential() {
                    Ok(()) | Err(keyring_core::Error::NoEntry) => {}
                    Err(error) => return Err(vault_error(&error)),
                }
            }
            repo.confirm_erased(slot)?;
        }
        Ok(())
    }
}

impl ManagedVault {
    fn entry(&self, slot: VaultSlot) -> Result<keyring_core::Entry> {
        self.platform
            .get_or_try_init(PlatformVault::new)
            .and_then(|store| {
                store.build(
                    KEYRING_SERVICE,
                    &format!("v2:{}:{}", self.namespace, slot.id),
                    None,
                )
            })
            .map_err(|error| vault_error(&error))
    }
}

fn vault_error(error: &keyring_core::Error) -> HostError {
    HostError::Unavailable(format!("credential store: {error}"))
}

/// Stores a secret in the keyring (or only in memory for a session-only host).
pub(crate) fn save_secret(
    credentials: &Credentials,
    connection: &str,
    secret: &str,
    expected_settings_revision: Option<u64>,
) -> Result<()> {
    if secret.is_empty() || secret.chars().count() > 8_192 {
        return Err(HostError::Invalid(
            ("credential must be 1-8192 characters").into(),
        ));
    }
    #[cfg(windows)]
    if !credentials.session_only() && secret.encode_utf16().count() > 1_280 {
        return Err(HostError::Invalid(
            "Windows の資格情報ストアに保存できる長さを超えています。".into(),
        ));
    }
    let Some(vault) = &credentials.vault else {
        #[cfg(test)]
        if credentials.session_only() {
            credentials.remember(connection, secret.to_owned());
            return Ok(());
        }
        return Err(HostError::Unavailable(
            "credential store is not initialized".into(),
        ));
    };
    let settings = vault.store.settings_repo();
    let (connection, created) = settings
        .connection_with_creation(connection)?
        .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
    validate_settings_revision(&settings, expected_settings_revision)?;
    let outcome = write_and_publish(credentials, vault, &connection, created, secret);
    credentials.recover_detached();
    outcome
}

fn write_and_publish(
    credentials: &Credentials,
    vault: &ManagedVault,
    connection: &Connection,
    created: u64,
    secret: &str,
) -> Result<()> {
    let _guard = vault.mutations.lock();
    let repo = vault.store.credential_repo();
    let slot = repo.stage(credentials.session_only())?;
    if slot.session_only {
        credentials
            .memory
            .lock()
            .session_slots
            .insert(slot.id, secret.to_owned());
    } else {
        vault
            .entry(slot)?
            .set_password(secret)
            .map_err(|error| vault_error(&error))?;
    }
    repo.publish(&connection.id, connection.revision, created, slot)?;
    credentials
        .memory
        .lock()
        .record_change(connection.id.as_str());
    Ok(())
}

pub(crate) fn delete_secret(credentials: &Credentials, connection: &str) -> Result<()> {
    if let Some(vault) = &credentials.vault {
        let (current, created) = vault
            .store
            .settings_repo()
            .connection_with_creation(connection)?
            .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
        let _guard = vault.mutations.lock();
        vault
            .store
            .credential_repo()
            .revoke(&current.id, current.revision, created)?;
    }
    credentials.forget(connection);
    credentials.recover_detached();
    Ok(())
}

pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
    providers: &crate::host::providers::Providers,
) -> Result<Option<String>> {
    if connection.credential_state == CredentialState::RegistrationRequired {
        return Err(HostError::FailedPrecondition(
            "この接続の認証情報を再登録してください。".into(),
        ));
    }
    let stored = credentials.map_or(Ok(None), |store| {
        store.read_secret(connection.id.as_str(), Some(&connection.config))
    });
    let managed = credentials.is_some_and(|store| store.vault.is_some());
    match stored {
        Ok(Some(secret)) => Ok(Some(secret)),
        Ok(None) if managed && connection.credential_state == CredentialState::Registered => Err(
            HostError::FailedPrecondition("登録済みの認証情報を読み込めません。".into()),
        ),
        Err(error) if managed || connection.credential_state == CredentialState::Registered => {
            Err(error)
        }
        unregistered => environment_secret(connection, providers)
            .map_or(unregistered, |secret| Ok(Some(secret))),
    }
}
