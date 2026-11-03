//! Wasm bridge: the Host owns session identity, permission waits and terminal state.
use super::*;
use crate::domain::harness::{CONTRACT_VERSION, Session, Status};
use ene_plugin::{HarnessEvent, HarnessInput};
use serde_json::Value;

/// A stored journal plus whether it matches the current contract.
pub(super) struct Loaded {
    pub(super) session: Session,
    pub(super) current: bool,
}

impl Manager {
    pub(super) async fn harness_load(&self, binding_id: &str) -> Result<Option<Loaded>> {
        let value = self
            .store
            .plugin_get("harness".into(), binding_id.into())
            .await
            .map_err(storage_failure)?;
        if value.is_null() {
            return Ok(None);
        }
        if let Ok(session) = serde_json::from_value::<Session>(value.clone())
            && session.contract_version == CONTRACT_VERSION
        {
            return Ok(Some(Loaded {
                session,
                current: true,
            }));
        }
        // A previous contract cannot be resumed: guest memory is gone and the
        // stored events use types this host no longer understands.
        let session_id = value
            .get("session_id")
            .or_else(|| value.get("task_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Ok(Some(Loaded {
            session: Session::interrupted(session_id),
            current: false,
        }))
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
                    .is_some_and(|loaded| !loaded.session.terminal())
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
                let Some(mut loaded) = self.harness_load(&binding.id).await? else {
                    return Err(conflict());
                };
                loaded.session.respond(request_id, outcome)?;
                self.harness_store(&binding.id, &loaded.session).await?;
                Ok(loaded.session)
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
        let mut loaded = self.harness_load(binding_id).await?.ok_or_else(conflict)?;
        if loaded.session.status == Status::Cancelled {
            return Ok(loaded.session);
        }
        if loaded.session.terminal() {
            return Err(conflict());
        }
        loaded.session.cancel();
        self.harness_store(binding_id, &loaded.session).await?;
        Ok(loaded.session)
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
        let Some(mut loaded) = self.harness_load(binding_id).await? else {
            return Ok(());
        };
        if loaded.session.terminal() {
            // A migrated journal still has to be rewritten in the current shape.
            return if loaded.current {
                Ok(())
            } else {
                self.harness_store(binding_id, &loaded.session).await
            };
        }
        if lifetime.is_cancelled() {
            loaded.session.fail("plugin_stopped");
        } else if cancel.is_cancelled() && !force_failure {
            loaded.session.cancel();
        } else {
            loaded.session.fail(reason);
        }
        self.harness_store(binding_id, &loaded.session).await
    }
    /// Records a terminal failure without touching the guest.
    pub(super) async fn harness_fail(&self, binding_id: &str, reason: &str) -> Result<()> {
        let Some(mut loaded) = self.harness_load(binding_id).await? else {
            return Ok(());
        };
        if loaded.session.terminal() {
            return Ok(());
        }
        loaded.session.fail(reason);
        self.harness_store(binding_id, &loaded.session).await
    }
    /// Binding/plugin lifecycle termination; `reason` is recorded as the failure.
    pub(super) async fn harness_stop(&self, binding_id: &str, reason: &str) -> Result<()> {
        let Some(mut loaded) = self.harness_load(binding_id).await? else {
            return Ok(());
        };
        if loaded.session.terminal() && loaded.current {
            return Ok(());
        }
        loaded.session.fail(reason);
        self.harness_store(binding_id, &loaded.session).await
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
