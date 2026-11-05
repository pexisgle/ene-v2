//! Wasm bridge: the Host owns session identity, permission waits and terminal state.
use super::*;
use crate::domain::harness::{CONTRACT_VERSION, Session, Status};
use ene_plugin::{HarnessEvent, HarnessInput};

impl Manager {
    /// A stored journal of another contract is not resumed. Guest memory is
    /// gone and the stored events use types this host no longer understands.
    pub(super) async fn harness_load(&self, binding_id: &str) -> Result<Option<Session>> {
        let value = self
            .store
            .plugin_get("harness".into(), binding_id.into())
            .await
            .map_err(storage_failure)?;
        if value.is_null() {
            return Ok(None);
        }
        let Ok(session) = serde_json::from_value::<Session>(value) else {
            return Ok(None);
        };
        Ok((session.contract_version == CONTRACT_VERSION).then_some(session))
    }
    /// Creates the session for `Start` or validates the pending request for
    /// `Respond`. The returned session is already durable.
    pub(super) async fn harness_begin(
        &self,
        binding: &PluginBinding,
        input: &HarnessInput,
    ) -> Result<Session> {
        match input {
            HarnessInput::Start { .. } => {
                if self
                    .harness_load(&binding.id)
                    .await?
                    .is_some_and(|session| !session.terminal())
                {
                    return Err(conflict());
                }
                let session = Session::start(uuid::Uuid::new_v4().to_string());
                self.harness_store(&binding.id, &session).await?;
                Ok(session)
            }
            HarnessInput::Respond {
                request_id,
                outcome,
            } => {
                let Some(mut session) = self.harness_load(&binding.id).await? else {
                    return Err(conflict());
                };
                session.respond(request_id, outcome)?;
                self.harness_store(&binding.id, &session).await?;
                Ok(session)
            }
            HarnessInput::Cancel => {
                unreachable!("cancel is handled before the worker lock is taken")
            }
        }
    }
    /// Applies and persists one event so long turns checkpoint their progress.
    pub(super) async fn harness_apply(
        &self,
        binding_id: &str,
        session: &mut Session,
        event: &HarnessEvent,
    ) -> Result<()> {
        session.apply(event)?;
        self.harness_store(binding_id, session).await
    }
    /// Terminates a suspended turn on behalf of the client. Never resumes the guest.
    pub(super) async fn harness_cancel(&self, binding_id: &str) -> Result<Session> {
        let mut session = self.harness_load(binding_id).await?.ok_or_else(conflict)?;
        if session.status == Status::Cancelled {
            return Ok(session);
        }
        if session.terminal() {
            return Err(conflict());
        }
        session.cancel();
        self.harness_store(binding_id, &session).await?;
        Ok(session)
    }
    /// Classifies and persists the terminal state of an unsuccessful invocation.
    pub(super) async fn harness_finish(
        &self,
        binding_id: &str,
        cancel: &CancellationToken,
        lifetime: &CancellationToken,
        reason: &str,
        force_failure: bool,
    ) -> Result<()> {
        let Some(mut session) = self.harness_load(binding_id).await? else {
            return Ok(());
        };
        if session.terminal() {
            return Ok(());
        }
        if lifetime.is_cancelled() {
            session.fail("plugin_stopped");
        } else if cancel.is_cancelled() && !force_failure {
            session.cancel();
        } else {
            session.fail(reason);
        }
        self.harness_store(binding_id, &session).await
    }
    /// Ends the session as a failure. Used by binding/plugin lifecycle teardown.
    pub(super) async fn harness_stop(&self, binding_id: &str, reason: &str) -> Result<()> {
        let Some(mut session) = self.harness_load(binding_id).await? else {
            return Ok(());
        };
        if session.terminal() {
            return Ok(());
        }
        session.fail(reason);
        self.harness_store(binding_id, &session).await
    }
    async fn harness_store(&self, binding_id: &str, session: &Session) -> Result<()> {
        self.store
            .plugin_set(
                "harness".into(),
                binding_id.into(),
                serde_json::to_value(session).map_err(failure)?,
            )
            .await
            .map_err(storage_failure)
    }
}
