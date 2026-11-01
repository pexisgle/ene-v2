//! Session registry for host bindings. Session state never outlives its binding.
use super::Manager;
use crate::ports::Result;
use ene_plugin_runtime::instance::Instance;
use ene_rpc::PluginBinding;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(PartialEq, Eq)]
struct SessionBinding {
    plugin_id: String,
    entrypoint: String,
    revision: String,
}
pub(super) struct SessionState {
    binding: SessionBinding,
    pub(super) lifetime: CancellationToken,
    pub(super) worker: Mutex<Option<Instance>>,
    pub(super) active: std::sync::Mutex<Option<CancellationToken>>,
}
impl SessionState {
    pub(super) fn new(binding: &PluginBinding, lifetime: CancellationToken) -> Self {
        Self {
            binding: SessionBinding {
                plugin_id: binding.plugin_id.clone(),
                entrypoint: binding.entrypoint.clone(),
                revision: binding.revision.clone(),
            },
            lifetime,
            worker: Mutex::new(None),
            active: std::sync::Mutex::new(None),
        }
    }
    pub(super) fn matches(&self, binding: &PluginBinding) -> bool {
        self.binding.plugin_id == binding.plugin_id
            && self.binding.entrypoint == binding.entrypoint
            && self.binding.revision == binding.revision
    }
}
pub(super) type Sessions = Arc<std::sync::Mutex<BTreeMap<String, Arc<SessionState>>>>;
type Registration = (Manager, String, Arc<SessionState>);
pub(super) struct Session<'a> {
    pub(super) guard: tokio::sync::MutexGuard<'a, Option<Instance>>,
    pub(super) retain: bool,
    pub(super) registration: Registration,
    pub(super) _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl std::ops::Deref for Session<'_> {
    type Target = Option<Instance>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}
impl std::ops::DerefMut for Session<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        if !self.retain {
            self.guard.take();
            let (manager, key, worker) = &self.registration;
            let mut sessions = manager.sessions.lock().unwrap();
            if sessions
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, worker))
            {
                sessions.remove(key);
            }
        }
    }
}
pub(super) struct ActiveInvocation {
    session: Arc<SessionState>,
    token: CancellationToken,
}
impl ActiveInvocation {
    pub(super) fn new(session: Arc<SessionState>, token: CancellationToken) -> Self {
        *session.active.lock().unwrap() = Some(token.clone());
        Self { session, token }
    }
}
impl Drop for ActiveInvocation {
    fn drop(&mut self) {
        // This guard is dropped before releasing the worker lock.
        *self.session.active.lock().unwrap() = None;
        self.token.cancel();
    }
}

/// One external ACP agent process for a binding.
pub(super) struct AcpSlot {
    pub(super) plugin_id: String,
    pub(super) cancel: CancellationToken,
    pub(super) session: tokio::sync::Mutex<Option<crate::infrastructure::acp::AcpSession>>,
}

impl Manager {
    pub(super) async fn stop_session(&self, key: &str, session: &Arc<SessionState>) -> Result<()> {
        session.lifetime.cancel();
        let mut worker = session.worker.lock().await;
        worker.take();
        if self
            .sessions
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            // Keep the stopped registration until its terminal journal is durable.
            self.harness_stop(key, "plugin_stopped").await?;
            let mut sessions = self.sessions.lock().unwrap();
            if sessions
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, session))
            {
                sessions.remove(key);
            }
        }
        Ok(())
    }
    pub(super) async fn stop_sessions(&self, package: Option<&str>) -> Result<()> {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| package.is_none_or(|p| p == session.binding.plugin_id))
            .map(|(key, session)| {
                session.lifetime.cancel();
                (key.clone(), session.clone())
            })
            .collect();
        let mut result = Ok(());
        for (key, session) in sessions {
            let stopped = self.stop_session(&key, &session).await;
            if result.is_ok() {
                result = stopped;
            }
        }
        let acp = self.stop_acp_sessions(package).await;
        if result.is_ok() {
            result = acp;
        }
        result
    }
}
