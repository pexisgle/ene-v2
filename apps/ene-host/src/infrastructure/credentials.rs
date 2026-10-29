//! Secret Service is isolated from application rules and HTTP input.
use crate::{
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
pub struct Store {
    writes: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Arc<AtomicBool>>>>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            writes: Arc::new(std::sync::Mutex::new(Default::default())),
            slots: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }
}
impl Credentials for Store {
    fn environment(&self, name: &str) -> Result<String> {
        std::env::var(name).map_err(|_| Error::new(ErrorKind::CredentialMissing))
    }
    fn session_only(&self) -> bool {
        std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some()
    }
    fn load(&self, reference: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let slot = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::unavailable())?;
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || {
                    let _slot = slot;
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
            let slot = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::unavailable())?;
            let finished = Arc::new(AtomicBool::new(false));
            self.writes
                .lock()
                .expect("credential writes")
                .insert(reference.clone(), finished.clone());
            let abandoned = Arc::new(AtomicBool::new(false));
            struct Abandonment(Arc<AtomicBool>, bool);
            impl Drop for Abandonment {
                fn drop(&mut self) {
                    if self.1 {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
            }
            let mut guard = Abandonment(abandoned.clone(), true);
            let cleanup = abandoned.clone();
            let target = reference.clone();
            let job = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                struct Finished(Arc<AtomicBool>);
                impl Drop for Finished {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _finished = Finished(finished);
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
                guard.1 = false;
                self.writes
                    .lock()
                    .expect("credential writes")
                    .remove(&reference);
                Ok(())
            } else {
                abandoned.store(true, Ordering::SeqCst);
                let _ = self.forget(reference).await;
                Err(Error::unavailable())
            }
        })
    }
    fn forget(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // A timed out blocking save can still finish later. Never report
            // NoEntry as successful cleanup while that write is in flight.
            if self
                .writes
                .lock()
                .expect("credential writes")
                .get(&reference)
                .is_some_and(|finished| !finished.load(Ordering::Acquire))
            {
                return Err(Error::unavailable());
            }
            let slot = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::unavailable())?;
            let target = reference.clone();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || {
                    let _slot = slot;
                    keyring::Entry::new("ene-v2", &target).and_then(|e| e.delete_credential())
                }),
            )
            .await;
            match result {
                Ok(Ok(Ok(()))) | Ok(Ok(Err(keyring::Error::NoEntry))) => {
                    self.writes
                        .lock()
                        .expect("credential writes")
                        .remove(&reference);
                    Ok(())
                }
                _ => Err(Error::unavailable()),
            }
        })
    }
}
