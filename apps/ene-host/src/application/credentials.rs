use super::{HostApplication, generation::*};
use crate::{
    domain::{rules, *},
    ports::Secret,
};
use uuid::Uuid;
#[derive(Default)]
pub(super) struct CleanupState {
    // true means this pass attempted the reference; failures stay observable.
    pub pending: std::collections::BTreeMap<String, bool>,
    worker: bool,
}
impl HostApplication {
    pub(super) fn schedule_forget(&self, reference: String) {
        let mut cleanup = self.0.cleanup.lock().expect("cleanup lock");
        cleanup.pending.insert(reference, false);
        if cleanup.worker {
            return;
        }
        cleanup.worker = true;
        let app = self.clone();
        self.0.cleanups.spawn(async move {
            use futures_util::FutureExt;
            loop {
                let reference = {
                    let mut cleanup = app.0.cleanup.lock().expect("cleanup lock");
                    let Some(reference) = cleanup
                        .pending
                        .iter()
                        .find(|(_, attempted)| !**attempted)
                        .map(|(reference, _)| reference.clone())
                    else {
                        cleanup.worker = false;
                        return;
                    };
                    cleanup.pending.insert(reference.clone(), true);
                    reference
                };
                for _ in 0..2 {
                    let deleted = std::panic::AssertUnwindSafe(async {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(6),
                            app.0.credentials.forget(reference.clone()),
                        )
                        .await
                    })
                    .catch_unwind()
                    .await;
                    if matches!(deleted, Ok(Ok(Ok(())))) {
                        app.0
                            .cleanup
                            .lock()
                            .expect("cleanup lock")
                            .pending
                            .remove(&reference);
                        break;
                    }
                }
            }
        });
    }

    pub(super) async fn register_credentials_inner(
        &self,
        id: String,
        value: Option<String>,
        source: Option<String>,
    ) -> Result<ene_rpc::CredentialResult, Error> {
        if self.0.cleanup.lock().expect("cleanup lock").pending.len() >= MAX_PENDING_MUTATIONS {
            return Err(Error::new(ErrorKind::ResourceLimit));
        }
        let environment = match source.as_deref() {
            None => false,
            Some("environment") if value.is_none() => true,
            _ => return Err(Error::invalid()),
        };
        let (value, environment_scope) = if environment {
            let mut controls = self.0.controls.lock().await;
            let c = config(self, &mut controls).await?;
            let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            let policy = self.0.providers.policy(&p)?;
            let name = policy
                .environment_variable
                .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
            (
                self.0.credentials.environment(&name)?,
                Some(policy.fingerprint),
            )
        } else {
            (value.ok_or_else(Error::invalid)?, None)
        };
        if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
            return Err(Error::invalid());
        }
        let session_only = environment || self.0.credentials.session_only();
        let nonce = Uuid::new_v4().to_string();
        let reference = Uuid::new_v4().to_string();
        let previous = {
            let mut controls = self.0.controls.lock().await;
            let mut c = config(self, &mut controls).await?;
            let mut p =
                rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            if environment_scope.as_ref().is_some_and(|scope| {
                !self
                    .0
                    .providers
                    .policy(&p)
                    .is_ok_and(|current| current.fingerprint == *scope)
            }) {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            let requested_state = if session_only {
                "session_only"
            } else {
                "stored"
            };
            if p.credential_state == requested_state
                && controls.secrets.get(&id).is_some_and(|s| s.value == value)
            {
                return Ok(ene_rpc::CredentialResult {
                    credential_state: p.credential_state,
                    ..Default::default()
                });
            }
            if active(&controls, &id) {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            p.credential_state = "session_only".into();
            let previous = c.credential_refs.remove(&id);
            update_connection(&mut c, p);
            let old_secret = controls.secrets.insert(
                id.clone(),
                Secret {
                    value: value.clone(),
                    nonce: nonce.clone(),
                },
            );
            if let Err(error) = commit(self, &mut controls, c).await {
                if let Some(secret) = old_secret {
                    controls.secrets.insert(id.clone(), secret);
                } else {
                    controls.secrets.remove(&id);
                }
                return Err(error);
            }
            previous
        };
        if let Some(old) = previous {
            self.schedule_forget(old);
        }
        if session_only {
            return Ok(ene_rpc::CredentialResult {
                credential_state: "session_only".into(),
                ..Default::default()
            });
        }
        let saved = self
            .0
            .credentials
            .save(reference.clone(), value)
            .await
            .is_ok();
        let mut controls = self.0.controls.lock().await;
        let mut c = config(self, &mut controls).await?;
        if !controls.secrets.get(&id).is_some_and(|s| s.nonce == nonce) {
            // The secret was stored for a candidate this request no longer owns.
            self.schedule_forget(reference);
            return Err(Error::new(ErrorKind::StateConflict));
        }
        if saved {
            let mut p =
                rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            p.credential_state = "stored".into();
            c.credential_refs.insert(id, reference.clone());
            update_connection(&mut c, p);
            if let Err(error) = commit(self, &mut controls, c).await {
                self.schedule_forget(reference);
                return Err(error);
            }
        } else {
            self.schedule_forget(reference);
        }
        Ok(ene_rpc::CredentialResult {
            credential_state: if saved { "stored" } else { "session_only" }.into(),
            ..Default::default()
        })
    }
    pub(super) async fn revoke_credentials_inner(&self, id: String) -> Result<(), Error> {
        let mut controls = self.0.controls.lock().await;
        let mut c = config(self, &mut controls).await?;
        let mut p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        cancel_connection(&controls, &id);
        controls.secrets.remove(&id);
        p.credential_state = "missing".into();
        if let Some(reference) = c.credential_refs.remove(&id) {
            self.schedule_forget(reference);
        }
        update_connection(&mut c, p);
        commit(self, &mut controls, c).await
    }
}
