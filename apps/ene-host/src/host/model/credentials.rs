//! Credential storage and lookup for provider connections.
//!
//! A registered secret lives in the OS keyring, or only in memory when the host
//! was told to keep sessions credential-free (`ENE_HOST_SESSION_CREDENTIALS`).
//! An unreadable store is reported as an error rather than "no credential", so a
//! request never goes out unauthenticated because the store could not be read.
use crate::host::db::Result;
use crate::host::error::HostError;
use crate::host::settings::{Connection, CredentialState};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};

const KEYRING_SERVICE: &str = "ene-host";

#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Credentials {
    memory: Arc<Mutex<CredentialMemory>>,
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
    entries: HashMap<String, CredentialEntry>,
}

struct CredentialEntry {
    generation: u64,
    session_secret: Option<String>,
}

impl Credentials {
    pub(crate) fn new(session_only: bool) -> Self {
        Self {
            session_only,
            ..Self::default()
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.memory.lock().generation
    }

    pub(crate) fn generation_of(&self, connection: &str) -> u64 {
        self.memory
            .lock()
            .entries
            .get(connection)
            .map_or(0, |entry| entry.generation)
    }

    fn record(&self, connection: &str, session_secret: Option<String>) {
        let mut memory = self.memory.lock();
        memory.generation += 1;
        let generation = memory.generation;
        memory.entries.insert(
            connection.to_owned(),
            CredentialEntry {
                generation,
                session_secret,
            },
        );
    }

    pub(crate) fn remember(&self, connection: &str, secret: String) {
        self.record(connection, Some(secret));
    }

    pub(crate) fn forget(&self, connection: &str) {
        self.record(connection, None);
    }

    /// The registered secret, `None` when none is registered. An unreadable
    /// credential store is an error, not "no credential": a request must not
    /// go out unauthenticated because the store could not be read.
    pub(crate) fn secret(&self, connection: &str) -> Result<Option<String>> {
        if self.session_only {
            return Ok(self
                .memory
                .lock()
                .entries
                .get(connection)
                .and_then(|slot| slot.session_secret.clone()));
        }
        #[cfg(test)]
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(HostError::Unavailable(
                "credential store: unreadable".into(),
            ));
        }
        read_keyring(connection)
    }

    #[cfg(test)]
    pub(crate) fn make_unreadable(&self) {
        self.unreadable
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn session_only(&self) -> bool {
        self.session_only
    }
}

fn keyring_entry(connection: &str) -> std::result::Result<keyring::Entry, HostError> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("connection:{connection}"))
        .map_err(|error| HostError::Unavailable(format!("credential store: {error}")))
}

fn read_keyring(connection: &str) -> std::result::Result<Option<String>, HostError> {
    match keyring_entry(connection)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(HostError::Unavailable(format!("credential store: {error}"))),
    }
}

/// Stores a secret in the keyring (or only in memory for a session-only host).
pub(crate) fn save_secret(credentials: &Credentials, connection: &str, secret: &str) -> Result<()> {
    if secret.is_empty() || secret.chars().count() > 8_192 {
        return Err(HostError::Invalid(
            ("credential must be 1-8192 characters").into(),
        ));
    }
    if !credentials.session_only() {
        keyring_entry(connection)?
            .set_password(secret)
            .map_err(|error| HostError::Unavailable(format!("credential store: {error}")))?;
    }
    if credentials.session_only() {
        credentials.remember(connection, secret.to_owned());
    } else {
        credentials.record(connection, None);
    }
    Ok(())
}

pub(crate) fn delete_secret(credentials: &Credentials, connection: &str) -> Result<()> {
    if !credentials.session_only() {
        match keyring_entry(connection)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(HostError::Unavailable(format!("credential store: {error}"))),
        }
    }
    credentials.forget(connection);
    Ok(())
}

/// The state the contract reports for a connection's secret.
pub(crate) fn credential_state(
    connection: &Connection,
    credentials: &Credentials,
    loaded: &crate::host::providers::Providers,
) -> Result<CredentialState> {
    if credentials.secret(connection.id.as_str())?.is_some() {
        return Ok(CredentialState::Registered);
    }
    if crate::host::settings::environment_variable(connection, loaded)
        .is_some_and(|name| std::env::var_os(name).is_some())
    {
        return Ok(CredentialState::Environment);
    }
    Ok(CredentialState::Absent)
}

pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
    loaded: &crate::host::providers::Providers,
) -> Result<Option<String>> {
    if let Some(credentials) = credentials
        && let Some(secret) = credentials.secret(connection.id.as_str())?
    {
        return Ok(Some(secret));
    }
    Ok(
        crate::host::settings::environment_variable(connection, loaded)
            .and_then(|name| std::env::var(name).ok())
            .filter(|secret| !secret.is_empty()),
    )
}
