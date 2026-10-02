use super::{HostApplication, generation::*};
use crate::{
    domain::{rules, *},
    ports::Secret,
};
use uuid::Uuid;
impl HostApplication {
    pub async fn register_credentials(
        &self,
        id: String,
        value: Option<String>,
        source: Option<String>,
    ) -> Result<ene_api_types::CredentialResult, Error> {
        let environment = match source.as_deref() {
            None => false,
            Some("environment") if value.is_none() => true,
            _ => return Err(Error::invalid()),
        };
        let value = if environment {
            let mut controls = self.0.controls.lock().await;
            let c = config(self, &mut controls).await?;
            let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            let name = self
                .0
                .providers
                .policy(&p)?
                .environment_variable
                .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
            self.0.credentials.environment(&name)?
        } else {
            value.ok_or_else(Error::invalid)?
        };
        if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
            return Err(Error::invalid());
        }
        let nonce = Uuid::new_v4().to_string();
        let reference = Uuid::new_v4().to_string();
        let previous = {
            let mut controls = self.0.controls.lock().await;
            let mut c = config(self, &mut controls).await?;
            let mut p =
                rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            if controls.secrets.get(&id).is_some_and(|s| s.value == value) {
                return Ok(ene_api_types::CredentialResult {
                    credential_state: p.credential_state,
                });
            }
            if active(&controls, &id) {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            p.credential_state = "session_only".into();
            let previous = c.credential_refs.remove(&id);
            update_connection(&mut c, p);
            commit(self, &mut controls, c).await?;
            controls.secrets.insert(
                id.clone(),
                Secret {
                    value: value.clone(),
                    nonce: nonce.clone(),
                },
            );
            previous
        };
        if let Some(old) = previous {
            self.0.credentials.forget(old);
        }
        if environment || self.0.credentials.session_only() {
            return Ok(ene_api_types::CredentialResult {
                credential_state: "session_only".into(),
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
            self.0.credentials.forget(reference);
            return Err(Error::new(ErrorKind::StateConflict));
        }
        if saved {
            let mut p =
                rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            p.credential_state = "stored".into();
            c.credential_refs.insert(id, reference.clone());
            update_connection(&mut c, p);
            if let Err(e) = commit(self, &mut controls, c).await {
                self.0.credentials.forget(reference);
                return Err(e);
            }
        }
        Ok(ene_api_types::CredentialResult {
            credential_state: if saved { "stored" } else { "session_only" }.into(),
        })
    }
    pub async fn revoke_credentials(&self, id: String) -> Result<(), Error> {
        let reference = {
            let mut controls = self.0.controls.lock().await;
            let mut c = config(self, &mut controls).await?;
            let mut p =
                rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            cancel_connection(&controls, &id);
            controls.secrets.remove(&id);
            p.credential_state = "missing".into();
            let reference = c.credential_refs.remove(&id);
            update_connection(&mut c, p);
            commit(self, &mut controls, c).await?;
            reference
        };
        if let Some(reference) = reference {
            self.0.credentials.forget(reference);
        }
        Ok(())
    }
}
