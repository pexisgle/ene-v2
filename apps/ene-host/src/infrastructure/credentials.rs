//! Secret Service is isolated from application rules and HTTP input.
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Credentials, Result},
};
use futures_util::future::BoxFuture;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
pub struct Store;
impl Credentials for Store {
    fn environment(&self, name: &str) -> Result<String> {
        std::env::var(name).map_err(|_| Error::new(ErrorKind::CredentialMissing))
    }
    fn session_only(&self) -> bool {
        std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some()
    }
    fn load(&self, reference: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || {
                    keyring::Entry::new("ene-v2", &reference).and_then(|e| e.get_password())
                }),
            )
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|r| r.ok())
            .ok_or_else(|| Error::new(ErrorKind::CredentialMissing))
        })
    }
    fn save(&self, reference: String, value: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let abandoned = Arc::new(AtomicBool::new(false));
            let cleanup = abandoned.clone();
            let target = reference.clone();
            let job = tokio::task::spawn_blocking(move || {
                keyring::Entry::new("ene-v2", &target).and_then(|e| {
                    let r = e.set_password(&value);
                    if cleanup.load(Ordering::SeqCst) {
                        let _ = e.delete_credential();
                    }
                    r
                })
            });
            if matches!(
                tokio::time::timeout(Duration::from_secs(5), job).await,
                Ok(Ok(Ok(())))
            ) {
                Ok(())
            } else {
                abandoned.store(true, Ordering::SeqCst);
                self.forget(reference);
                Err(Error::unavailable())
            }
        })
    }
    fn forget(&self, reference: String) {
        tokio::task::spawn_blocking(move || {
            let _ = keyring::Entry::new("ene-v2", &reference).and_then(|e| e.delete_credential());
        });
    }
}
