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
/// A platform credential call that outlives this deadline is reported as
/// unavailable; the blocking thread still owns the slot until it returns.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Keyring operations are serialized: the platform stores are blocking and a
/// second same-key write would otherwise race the first one's cleanup.
pub struct Store {
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(1)),
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
            // Loads wait for the single slot instead of being skipped: startup
            // loads several credentials concurrently and must not lose the
            // ones that do not win a try_acquire race.
            //
            // A platform call that hangs keeps the slot until the blocking
            // thread returns, so the deadline covers the wait for the slot and
            // the call itself instead of bounding them separately.
            let deadline = tokio::time::Instant::now() + TIMEOUT;
            let slot = tokio::time::timeout_at(deadline, self.slots.clone().acquire_owned())
                .await
                .map_err(|_| Error::unavailable())?
                .map_err(|_| Error::unavailable())?;
            tokio::time::timeout_at(
                deadline,
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
                keyring::Entry::new("ene-v2", &target).and_then(|e| {
                    let r = e.set_password(&value);
                    if cleanup.load(Ordering::SeqCst) {
                        let _ = e.delete_credential();
                    }
                    r
                })
            });
            if matches!(tokio::time::timeout(TIMEOUT, job).await, Ok(Ok(Ok(())))) {
                guard.1 = false;
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
            // A timed out blocking save can still hold the single slot, so a
            // concurrent forget must not report NoEntry as successful cleanup.
            let slot = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::unavailable())?;
            let target = reference.clone();
            let result = tokio::time::timeout(
                TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    let _slot = slot;
                    keyring::Entry::new("ene-v2", &target).and_then(|e| e.delete_credential())
                }),
            )
            .await;
            match result {
                Ok(Ok(Ok(()))) | Ok(Ok(Err(keyring::Error::NoEntry))) => Ok(()),
                _ => Err(Error::unavailable()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_load_and_forget_use_the_platform_store_in_sequence() {
        // Keyring v1 installs the platform store on the first `Entry::new` and
        // replaces any default store, so force that once before mocking it.
        // A host without a credential store cannot run this path at all.
        if keyring::Entry::store_status().is_err() {
            return;
        }
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        let store = Store::default();
        store.save("ref".into(), "secret".into()).await.unwrap();
        assert_eq!(store.load("ref".into()).await.unwrap(), "secret");
        store.forget("ref".into()).await.unwrap();
        assert!(store.load("ref".into()).await.is_err());

        for index in 0..8 {
            store
                .save(format!("ref-{index}"), format!("secret-{index}"))
                .await
                .unwrap();
        }
        let loads = (0..8).map(|index| store.load(format!("ref-{index}")));
        let loaded = futures_util::future::join_all(loads).await;
        assert!(loaded.iter().all(|item| item.is_ok()));
        keyring_core::unset_default_store();
    }

    /// A platform call that never releases its slot must not make the next
    /// load wait forever: the deadline covers the wait as well as the call.
    #[tokio::test(start_paused = true)]
    async fn a_load_behind_a_stuck_slot_fails_instead_of_waiting_forever() {
        let store = Store::default();
        let stuck = store.slots.clone().acquire_owned().await.unwrap();
        let error = store.load("ref".into()).await.expect_err("no result");
        assert_eq!(error.kind, ErrorKind::Unavailable);
        drop(stuck);
        assert!(store.load("ref".into()).await.is_err());
    }
}
