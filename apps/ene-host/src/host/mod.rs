//! The host: one headless Rig app on a thread of its own, the SQLite state and
//! the notification outbox.
//!
//! Every RPC operation is serialized onto the app thread, so SQLite, the ECS
//! world and the conversation log never race. Reads do not advance the agent
//! loop; mutations do, and the frame's notifications are flushed after it.
pub(crate) mod agent;
pub(crate) mod autonomy;
pub(crate) mod background;
pub(crate) mod chat;
pub(crate) mod client;
pub(crate) mod compaction;
pub(crate) mod db;
pub(crate) mod delegation;
pub(crate) mod error;
pub(crate) mod eyes;
pub(crate) mod mcp;
pub(crate) mod mind;
pub(crate) mod model;
pub(crate) mod net;
pub(crate) mod notify;
pub(crate) mod permission;
pub(crate) mod providers;
pub(crate) mod resources;
pub(crate) mod settings;
pub(crate) mod tools;
pub(crate) mod voice;
pub(crate) mod web_search;

#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod wire;
// Split out of `wire` when it outgrew 500 lines; `wire` re-exports its
// `settings`, `check` and `connection_from` so `host::wire::` paths hold.
pub(crate) mod work;

// Split out of this file when it outgrew 500 lines. `snapshot` and `subscribe`
// keep their `host::` paths for the RPC layer.
mod kernel;
mod runtime_start;
mod services;
pub(crate) use services::HostServices;
mod snapshot;

pub(crate) use snapshot::SnapshotReader;
#[cfg(test)]
mod runtime_tests;

use self::{
    db::Store,
    error::{HostError, Result},
    model::Credentials,
    notify::Notifier,
    permission::Approvals,
    resources::{DeviceHub, DeviceTimeout, HostRuntime, HostState, Options, Perception, Waiting},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use parking_lot::Mutex;
use rig_ecs::{prelude::*, session_log::SessionLog, session_log::SessionStore};
use rig_memory::DirStore;
use std::{path::Path, sync::Arc, thread::JoinHandle, time::Duration};
use tokio::sync::{broadcast, oneshot};

const QUEUE: usize = 256;
const MAX_COMPANIONS: usize = 16;

/// Why the host could not come up.
///
/// A caller can match on this instead of downcasting a `Box<dyn Error>`, and the
/// `thiserror` messages name the step that failed rather than repeating the
/// whole chain as one opaque string.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StartError {
    /// Opening or migrating the SQLite store failed.
    #[error("cannot prepare the store: {0}")]
    Store(#[from] HostError),
    /// A repository read or write during startup failed.
    #[error("cannot seed the store: {0}")]
    Seed(#[from] ene_store::StoreError),
    /// Opening the Rig session-log directory failed.
    ///
    /// The source is boxed because `rig-memory` does not re-export its
    /// `SessionError`, so naming the concrete type here would need a private path.
    #[error("cannot open the session log at {path}: {source}")]
    SessionLog {
        path: std::path::PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The kernel thread never reported ready.
    #[error("the agent kernel did not start")]
    Kernel,
}

type Job = Box<dyn FnOnce(&mut App) + Send>;

/// Runs `job` on the actor thread inside the span its caller was already in.
///
/// The actor thread is not the thread that made the request, so `Span::current()`
/// there is empty. Capturing it at submit time and entering it here is what lets
/// one trace connect an RPC to the actor work it caused.
fn trace_job(span: tracing::Span, job: Job) -> Job {
    Box::new(move |app| {
        let _entered = span.enter();
        job(app);
    })
}

enum Step {
    Job(Job),
    Wake,
    Stop,
}

/// The connection.json endpoint and the host's Owner Bearer stay the only
/// authentication; see docs/design/host-access.md.
#[derive(Clone)]
pub(crate) struct Runtime {
    handle: Arc<Handle>,
    services: Arc<HostServices>,
}

struct Handle {
    commands: crossbeam_channel::Sender<Job>,
    shutdown: crossbeam_channel::Sender<()>,
    thread: Mutex<Option<JoinHandle<Result<()>>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        ene_domain::debug_if_err(self.shutdown.try_send(()), "host shutdown signal");
        if let Some(thread) = self.thread.get_mut().take() {
            drop(thread.join());
        }
    }
}

impl Runtime {
    // The remaining `Runtime` methods. `start` and `start_with` are in
    // `runtime_start.rs`, which re-opens this impl block.
    /// Tries to enqueue `operation` immediately. Accepted jobs advance one
    /// frame and flush notifications even if the reply future is dropped.
    pub(crate) fn call<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut App) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.send(move |reply| {
            Box::new(move |app| {
                let result = services.health.ready().and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), &error.to_string());
                }
                tick(app);
                let result = services.health.ready().and(result);
                let _reply = reply.send(result);
            })
        })
    }

    /// Tries to enqueue `operation` immediately. Accepted jobs run without
    /// advancing the agent loop even if the reply future is dropped.
    pub(crate) fn read<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut App) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.send(move |reply| {
            Box::new(move |app| {
                let result = services.health.ready().and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), &error.to_string());
                }
                let _reply = reply.send(result);
            })
        })
    }

    /// Runs an operation on live agents, then advances the kernel.
    pub(crate) fn call_kernel<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut agent::AgentKernel<'_>) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.call(move |app| operation(&mut agent::AgentKernel::new(app, &services)))
    }

    /// Reads live agent state between ticks.
    pub(crate) fn read_kernel<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut agent::AgentKernel<'_>) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.read(move |app| operation(&mut agent::AgentKernel::new(app, &services)))
    }

    /// Serializes a service mutation with kernel ticks without exposing ECS.
    pub(crate) fn change<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&HostServices) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.call(move |_| operation(&services))
    }

    /// Groups a read with the current notification cursor on the kernel thread.
    pub(crate) fn inspect<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&HostServices) -> Result<T> + Send + 'static,
    {
        let services = Arc::clone(&self.services);
        self.read(move |_| operation(&services))
    }

    fn send<T: Send + 'static, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(oneshot::Sender<Result<T>>) -> Job,
    {
        let (reply, received) = oneshot::channel();
        let submitted = self
            .handle
            .commands
            .try_send(trace_job(tracing::Span::current(), job(reply)))
            .map_err(|_| HostError::Exhausted(("host command queue is full or closed").into()));
        async move {
            submitted?;
            received
                .await
                .map_err(|_| HostError::Unavailable(("Rig kernel stopped").into()))?
        }
    }

    /// The same readiness predicate the RPC layer applies, for `/readyz`.
    pub(crate) fn ready(&self) -> Result<()> {
        self.services.health.ready()
    }

    pub(crate) async fn begin_shutdown(&self) {
        // A shutdown that cannot mark itself closing still proceeds; the
        // failure is reported so a wedged kernel is visible rather than silent.
        ene_domain::warn_if_err(
            self.inspect(|services| {
                services.health.close();
                Ok(())
            })
            .await,
            "host closing flag",
        );
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.services.voicevox.stop().await;
        let shutdown = self.handle.shutdown.clone();
        let thread = self.handle.thread.lock().take();
        tokio::task::spawn_blocking(move || {
            thread.map_or(Ok(()), |thread| {
                ene_domain::debug_if_err(shutdown.try_send(()), "host shutdown signal");
                thread
                    .join()
                    .map_err(|_| HostError::Internal(("Rig kernel panicked").into()))?
            })
        })
        .await
        .map_err(|_| HostError::Internal(("Rig kernel join failed").into()))?
    }
}

fn next(
    commands: &crossbeam_channel::Receiver<Job>,
    wake: &crossbeam_channel::Receiver<()>,
    shutdown: &crossbeam_channel::Receiver<()>,
) -> Step {
    // Cancellation requests complete on worker threads; a frame every second
    // keeps deadlines and timeouts moving while nobody calls the API.
    crossbeam_channel::select_biased! {
        recv(shutdown) -> _ => Step::Stop,
        recv(commands) -> job => match job {
            Ok(job) => Step::Job(job),
            Err(_) => Step::Stop,
        },
        recv(wake) -> _ => Step::Wake,
        default(Duration::from_secs(1)) => Step::Wake,
    }
}

/// One frame: run the app, notice a lost session log, and flush notifications.
pub(crate) fn tick(app: &mut App) {
    app.update();
    if let Some(reason) = app.world().resource::<SessionLog>().failure() {
        fail(app.world_mut(), &reason);
    }
    if app.world().resource::<HostState>().failure().is_some() {
        app.world().resource::<Notifier>().clear();
        return;
    }
    app.world().resource::<Notifier>().flush();
}

/// Applies a fatal error at the same boundary as queued ECS work.
pub(crate) fn fail(world: &mut World, reason: &str) {
    ene_domain::debug_if_err(
        world.run_system_cached_with(stop_on_failure, reason.to_owned()),
        "stop agents after host failure",
    );
}

pub(crate) fn stop_on_failure(
    In(reason): In<String>,
    state: Option<Res<HostState>>,
    notifier: Option<Res<Notifier>>,
    agents: Query<Entity, With<Agent>>,
    mut commands: Commands,
) {
    if state.is_some_and(|state| !state.fail(&reason)) {
        return;
    }
    tracing::error!(%reason, "the host stopped");
    for entity in &agents {
        commands.trigger(StopTurn::plugin(entity, "ene-host", "host failure"));
    }
    if let Some(notifier) = notifier {
        notifier.fail(&reason);
    }
}

/// Restores state that must not wait for the ECS frame: stale approvals and
/// operations whose answers can no longer arrive.
fn restore(store: &Store) -> Result<()> {
    store.approval_repo().expire_pending_approvals()?;
    store
        .client_repo()
        .mark_unanswered_operations_uncertain(None)?;
    Ok(())
}

/// Restores what needs a restorable session: the agents, their models and the
/// capability tools. It runs after the first frame, so the kernel's restore and
/// its startup systems are done before the host adopts or retires an agent.
fn restore_after_first_frame(world: &mut World) -> Result<()> {
    work::restore(world)?;
    agent::reconcile(world)?;
    // The parents exist once the reconciliation has bound them, so publishing
    // now puts a restored active client's tools on every parent before the
    // first turn.
    client::dynamic::init(world);
    Ok(())
}

/// The whole public state, used for `GetSnapshot` and `Subscribe`.
fn maybe_auto_start_voicevox(
    store: &Store,
    providers: &providers::Providers,
    voicevox: &voice::voicevox::VoicevoxManager,
) {
    if !cfg!(test)
        && let Ok(settings) = store.settings_repo().settings()
        && let Some(vv) = settings.connections.iter().find(|conn| {
            conn.enabled
                && conn.kind == settings::ConnectionKind::Tts
                && conn.endpoint().adapter.eq_ignore_ascii_case("voicevox")
                && !model::is_sakura_endpoint(&conn.endpoint().endpoint)
                && !providers.claims("voicevox", ene_plugin::Extension::Tts)
        })
    {
        let mgr = voicevox.clone();
        let base = vv.endpoint().endpoint.clone();
        let options = vv.endpoint().options.clone();
        tokio::spawn(async move {
            if let Err(error) = mgr.ensure_running(&base, &options).await {
                tracing::warn!(%error, "could not auto-start VOICEVOX engine at startup");
            }
        });
    }
}
