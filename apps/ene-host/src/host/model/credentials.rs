//! Host-owned credential storage backed by the OS keyring and a session cache.
use super::builtin;
use crate::host::{
    db::{Result, Store},
    error::HostError,
    settings::Connection,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

const SERVICE: &str = "ene-v2";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct Credentials {
    slots: Arc<Semaphore>,
    cache: Arc<Mutex<HashMap<String, String>>>,
    session_only: bool,
}

impl Credentials {
    pub(crate) async fn load(store: &Store) -> Self {
        let credentials = Self {
            slots: Arc::new(Semaphore::new(1)),
            cache: Arc::new(Mutex::new(HashMap::new())),
            session_only: std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some(),
        };
        let refs: Vec<String> = store
            .settings()
            .unwrap_or_else(|_| crate::host::settings::Settings {
                revision: 1,
                connections: Vec::new(),
                updated_at: crate::host::db::now(),
            })
            .connections
            .into_iter()
            .filter_map(|connection| connection.credential_ref)
            .collect();
        for reference in refs {
            if let Ok(secret) = credentials.read(&reference).await
                && let Ok(mut cache) = credentials.cache.lock()
            {
                cache.insert(reference, secret);
            }
        }
        credentials
    }

    pub(crate) fn cached(&self, reference: &str) -> Option<String> {
        self.cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(reference).cloned())
    }

    pub(crate) fn environment(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|secret| !secret.is_empty())
    }

    pub(crate) async fn save(&self, reference: &str, secret: &str) -> Result<()> {
        if secret.is_empty() {
            return Err(HostError::unavailable("credential is empty"));
        }
        if !self.session_only {
            let value = secret.to_owned();
            self.keyring(
                reference,
                move |entry| entry.set_password(&value),
                "credential store is unavailable",
            )
            .await?;
        }
        self.cache
            .lock()
            .map_err(|_| HostError::internal("credential cache is poisoned"))?
            .insert(reference.to_owned(), secret.to_owned());
        Ok(())
    }

    pub(crate) async fn forget(&self, reference: &str) -> Result<()> {
        self.cache
            .lock()
            .map_err(|_| HostError::internal("credential cache is poisoned"))?
            .remove(reference);
        if self.session_only {
            return Ok(());
        }
        self.keyring(
            reference,
            |entry| match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error),
            },
            "credential store cleanup failed",
        )
        .await
    }

    async fn read(&self, reference: &str) -> Result<String> {
        if let Some(secret) = self.cached(reference) {
            return Ok(secret);
        }
        self.keyring(
            reference,
            |entry| entry.get_password(),
            "credential is missing",
        )
        .await
    }

    async fn keyring<T: Send + 'static>(
        &self,
        reference: &str,
        operation: impl FnOnce(keyring::Entry) -> keyring::Result<T> + Send + 'static,
        message: &'static str,
    ) -> Result<T> {
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| HostError::unavailable("credential store is busy"))?;
        let key = reference.to_owned();
        let outcome = tokio::time::timeout(
            TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                keyring::Entry::new(SERVICE, &key).and_then(operation)
            }),
        )
        .await;
        match outcome {
            Ok(Ok(Ok(value))) => Ok(value),
            _ => Err(HostError::unavailable(message)),
        }
    }
}

pub(crate) fn secret(credentials: &Credentials, record: &Connection) -> Option<String> {
    record
        .credential_ref
        .as_deref()
        .and_then(|reference| credentials.cached(reference))
        .or_else(|| {
            let provider = builtin(&record.adapter)?;
            let endpoint = record.endpoint.trim_end_matches('/');
            (endpoint == provider.endpoint)
                .then(|| credentials.environment(provider.environment))
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_only_credentials_stay_in_memory() {
        let credentials = Credentials {
            slots: Arc::new(Semaphore::new(1)),
            cache: Arc::new(Mutex::new(HashMap::new())),
            session_only: true,
        };
        credentials.save("reference", "secret").await.unwrap();
        assert_eq!(credentials.cached("reference").as_deref(), Some("secret"));
        credentials.forget("reference").await.unwrap();
        assert!(credentials.cached("reference").is_none());
    }
}
