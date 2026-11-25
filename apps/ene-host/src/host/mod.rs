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
pub(crate) mod json;
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
pub(crate) mod util;
pub(crate) mod voice;
pub(crate) mod web_search;

#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod wire;
// Split out of `wire` when it outgrew 500 lines; `wire` re-exports its
// `settings`, `check` and `connection_from` so `host::wire::` paths hold.
mod wire_settings;
pub(crate) mod work;

// Split out of this file when it outgrew 500 lines. `snapshot` and `subscribe`
// keep their `host::` paths for the RPC layer.
mod kernel;
mod runtime_start;
mod snapshot;

pub(crate) use kernel::subscribe;
pub(crate) use snapshot::snapshot;
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
use rig_ecs::{prelude::*, session_log::SessionLog, session_log::SessionStore};
use rig_memory::DirStore;
use std::{
    error::Error,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

const QUEUE: usize = 256;
const MAX_COMPANIONS: usize = 16;

/// Locks `mutex` and takes the guard back from a poisoned one. A panic that
/// unwound while a lock was held leaves the guarded value whole, so the host
/// recovers the lock instead of failing every later access to it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type StartError = Box<dyn Error + Send + Sync>;
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
    state_dir: std::path::PathBuf,
    /// The relational store, held directly so an operation that needs only
    /// storage does not have to reach through the world for it.
    store: Store,
    credentials: Credentials,
    hub: DeviceHub,
    providers: providers::Providers,
    voicevox: voice::voicevox::VoicevoxManager,
}

struct Handle {
    commands: crossbeam_channel::Sender<Job>,
    shutdown: crossbeam_channel::Sender<()>,
    thread: Mutex<Option<JoinHandle<Result<()>>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _shutdown = self.shutdown.try_send(());
        if let Ok(mut thread) = self.thread.lock()
            && let Some(thread) = thread.take()
        {
            let _joined = thread.join();
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
        self.send(move |reply| {
            Box::new(move |app| {
                let result = app
                    .world()
                    .resource::<HostState>()
                    .ready()
                    .and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), error.to_string());
                }
                tick(app);
                let result = app.world().resource::<HostState>().ready().and(result);
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
        self.send(move |reply| {
            Box::new(move |app| {
                let result = app
                    .world()
                    .resource::<HostState>()
                    .ready()
                    .and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), error.to_string());
                }
                let _reply = reply.send(result);
            })
        })
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
    ///
    /// It reads the world rather than a separate flag, so a host that answers
    /// `/readyz` with 200 is one whose RPC calls would also be accepted.
    pub(crate) async fn ready(&self) -> Result<()> {
        self.read(|app| app.world().resource::<HostState>().ready())
            .await
    }

    pub(crate) async fn begin_shutdown(&self) {
        // A shutdown that cannot mark itself closing still proceeds; the
        // failure is reported so a wedged kernel is visible rather than silent.
        ene_platform_os::warn_if_err(
            self.call(|app| {
                app.world_mut().resource_mut::<HostState>().closing = true;
                Ok(())
            })
            .await,
            "host closing flag",
        );
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.voicevox.stop().await;
        let shutdown = self.handle.shutdown.clone();
        let thread = self
            .handle
            .thread
            .lock()
            .map_err(|_| HostError::Internal(("Rig kernel lock failed").into()))?
            .take();
        tokio::task::spawn_blocking(move || {
            thread.map_or(Ok(()), |thread| {
                let _shutdown = shutdown.try_send(());
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
        fail(app.world_mut(), reason);
    }
    if app.world().resource::<HostState>().failed.is_some() {
        app.world_mut().resource_mut::<Notifier>().clear();
        return;
    }
    app.world_mut().resource_mut::<Notifier>().flush();
}

/// A sticky host failure: it stops the agent loop and refuses new work.
pub(crate) fn fail(world: &mut World, reason: String) {
    if world
        .get_resource::<HostState>()
        .is_some_and(|state| state.failed.is_some())
    {
        return;
    }
    tracing::error!(%reason, "the host stopped");
    let agents: Vec<Entity> = world
        .query_filtered::<Entity, With<Agent>>()
        .iter(world)
        .collect();
    for agent in agents {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some("host failure".into()),
        });
    }
    if let Some(mut state) = world.get_resource_mut::<HostState>() {
        state.failed = Some(reason.clone());
    }
    if let Some(mut notifier) = world.get_resource_mut::<Notifier>() {
        notifier.fail(&reason);
    }
}

/// Restores state that must not wait for the ECS frame: stale approvals and
/// operations whose answers can no longer arrive.
fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store.approval_repo().expire_pending_approvals()?;
    store.client_repo().mark_unanswered_operations_uncertain()?;
    Ok(())
}

/// Restores what needs a restorable session: the agents, their models and the
/// capability tools. It runs after the first frame, so the kernel's restore and
/// its startup systems are done before the host adopts or retires an agent.
fn restore_after_first_frame(world: &mut World) -> Result<()> {
    agent::reconcile(world)?;
    // The parents exist once the reconciliation has bound them, so publishing
    // now puts a restored active client's tools on every parent before the
    // first turn.
    client::dynamic::init(world);
    Ok(())
}

fn boxed(error: impl std::fmt::Display) -> StartError {
    std::io::Error::other(error.to_string()).into()
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
                && providers.get("voicevox").is_none()
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
