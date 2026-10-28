//! One subscription owns buffering, snapshots, event ordering and history reconciliation.
use super::{Http, call_options};
use connectrpc::ConnectError as Error;
use ene_rpc::{self as types, *};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

struct Reader {
    stop: watch::Sender<bool>,
    job: Option<tokio::task::JoinHandle<()>>,
    events: mpsc::Receiver<HostEvent>,
    overflow: Arc<AtomicBool>,
    terminal: Arc<std::sync::Mutex<Option<Error>>>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl Reader {
    fn start(mut stream: RpcStream) -> Self {
        let (sender, events) = mpsc::channel(256);
        let (stop, mut stopping) = watch::channel(false);
        let overflow = Arc::new(AtomicBool::new(false));
        let exceeded = overflow.clone();
        let terminal = Arc::new(std::sync::Mutex::new(None));
        let terminal_error = terminal.clone();
        let job = tokio::spawn(async move {
            loop {
                let event = tokio::select! { biased;
                    _ = stopping.changed() => break,
                    event = stream.message::<HostEvent>() => event,
                };
                let event = match event {
                    Ok(Some(event)) => event.to_owned_message(),
                    Ok(None) => break,
                    Err(error) => {
                        *terminal_error.lock().expect("reader terminal") = Some(error);
                        break;
                    }
                };
                let resync = matches!(event.event, Some(host_event::Event::ResyncRequired(_)));
                if sender.try_send(event).is_err() {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
                if resync {
                    break;
                }
            }
        });
        Self {
            stop,
            job: Some(job),
            events,
            overflow,
            terminal,
        }
    }
}

/// Mutable client state indexes generated messages; it is never a wire representation.
#[derive(Clone)]
struct SessionState {
    cursor: Cursor,
    messages: BTreeMap<String, Message>,
}
impl SessionState {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            cursor: snapshot.cursor.as_option().cloned().unwrap_or_default(),
            messages: snapshot
                .messages
                .iter()
                .map(|message| (message.id.clone(), message.clone()))
                .collect(),
        }
    }
    /// Covered events are ignored. Invalid events leave the cursor and messages unchanged.
    fn apply(&mut self, notification: &Notification) -> Result<bool, Error> {
        let cursor = notification
            .cursor
            .as_option()
            .ok_or(Error::failed_precondition(
                "subscription must be resynchronized",
            ))?;
        if cursor.epoch != self.cursor.epoch {
            return Err(Error::failed_precondition(
                "subscription must be resynchronized",
            ));
        }
        if cursor.seq <= self.cursor.seq {
            return Ok(false);
        }
        if self.cursor.seq.checked_add(1) != Some(cursor.seq) {
            return Err(Error::failed_precondition(
                "subscription must be resynchronized",
            ));
        }
        use types::notification::Event;
        match notification.event.as_ref() {
            Some(
                Event::MessageCreated(message)
                | Event::MessageStarted(message)
                | Event::MessageUpdated(message),
            ) => {
                if notification.companion_id.as_deref() != Some(&message.companion_id) {
                    return Err(Error::failed_precondition(
                        "subscription must be resynchronized",
                    ));
                }
                self.messages
                    .insert(message.id.clone(), (**message).clone());
            }
            Some(Event::MessageDelta(delta)) => {
                let message =
                    self.messages
                        .get_mut(&delta.message_id)
                        .ok_or(Error::failed_precondition(
                            "subscription must be resynchronized",
                        ))?;
                if notification.companion_id.as_deref() != Some(&message.companion_id)
                    || message.status != "streaming"
                    || u64::try_from(message.text.chars().count()).ok() != Some(delta.offset)
                {
                    return Err(Error::failed_precondition(
                        "subscription must be resynchronized",
                    ));
                }
                message.text.push_str(&delta.delta);
            }
            // Unknown future event fields still consume their cursor.
            _ => {}
        }
        self.cursor = cursor.clone();
        Ok(true)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub host_id: String,
    pub epoch: String,
    pub subscription: String,
}
struct SessionView {
    snapshot: Snapshot,
    generation: u64,
}
/// Capture this together with the paging cursor before starting the HTTP request.
#[derive(Clone, Debug)]
pub struct HistoryRequest {
    pub request: types::HistoryRequest,
    generation: u64,
    identity: SessionIdentity,
}
/// Applied history metadata, without another copy of the message page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryApplied {
    pub added_rows: bool,
    pub next_cursor: Option<String>,
}
enum SyncCommand {
    Refresh(tokio::sync::oneshot::Sender<Result<(), Error>>),
    History(
        Box<HistoryRequest>,
        History,
        tokio::sync::oneshot::Sender<Result<HistoryApplied, Error>>,
    ),
}

/// The worker owns application of events. Cancelling a waiter cannot consume half an update.
pub struct SyncedSession {
    identity: SessionIdentity,
    view: Arc<std::sync::Mutex<SessionView>>,
    commands: mpsc::Sender<SyncCommand>,
    notices: mpsc::Receiver<Notification>,
    failure: watch::Receiver<Option<Error>>,
    stop: watch::Sender<bool>,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for SyncedSession {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl SyncedSession {
    pub async fn connect(http: Http) -> Result<SyncedSession, Error> {
        let mut stream = timeout(Duration::from_secs(5), http.subscribe(Empty::default()))
            .await
            .map_err(|_| Error::unavailable("request transport failed"))??;
        let first = timeout(Duration::from_secs(5), stream.message::<HostEvent>())
            .await
            .map_err(|_| Error::unavailable("request transport failed"))??
            .ok_or(Error::internal("invalid API response"))?
            .to_owned_message();
        let Some(host_event::Event::Initial(initial)) = first.event else {
            return Err(Error::internal("invalid API response"));
        };
        if initial.subscription_id.is_empty() {
            return Err(Error::internal("invalid API response"));
        }
        let subscription = initial.subscription_id;
        let snapshot = initial
            .snapshot
            .into_option()
            .ok_or(Error::internal("invalid API response"))?;
        let epoch = snapshot
            .cursor
            .as_option()
            .ok_or(Error::internal("invalid API response"))?
            .epoch
            .clone();
        let reader = Reader::start(stream);
        let identity = SessionIdentity {
            host_id: snapshot.host_id.clone(),
            epoch,
            subscription: subscription.clone(),
        };
        let view = Arc::new(std::sync::Mutex::new(published_view(
            &snapshot,
            &SessionState::from_snapshot(&snapshot),
            0,
        )));
        let (commands, receiver) = mpsc::channel(64);
        let (notices, incoming) = mpsc::channel(256);
        let (failure, failed) = watch::channel(None);
        let (stop, stopping) = watch::channel(false);
        let worker_view = view.clone();

        let worker = tokio::spawn(async move {
            let result = run_worker(
                http,
                subscription,
                snapshot,
                reader,
                receiver,
                notices,
                worker_view,
                stopping,
            )
            .await;
            if let Err(error) = result {
                failure.send_replace(Some(error));
            }
        });
        Ok(SyncedSession {
            identity,
            view,
            commands,
            notices: incoming,
            failure: failed,
            stop,
            worker: Some(worker),
        })
    }
}
impl SyncedSession {
    pub fn identity(&self) -> &SessionIdentity {
        &self.identity
    }
    /// All messages and the cursor describe the same published state.
    pub fn snapshot(&self) -> Snapshot {
        self.view.lock().expect("session view").snapshot.clone()
    }
    pub fn history_request(&self, companion: &str) -> HistoryRequest {
        let view = self.view.lock().expect("session view");
        HistoryRequest {
            request: types::HistoryRequest {
                companion_id: companion.into(),
                cursor: history_cursor(&view.snapshot, companion),
                ..Default::default()
            },
            generation: view.generation,
            identity: self.identity.clone(),
        }
    }
    pub async fn next(&mut self) -> Result<Notification, Error> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        tokio::select! {
            biased;
            changed = self.failure.changed() => {
                let _ = changed;
                Err(self.failure.borrow().clone().unwrap_or(Error::unavailable("request transport failed")))
            }
            notice = self.notices.recv() => notice.ok_or(Error::unavailable("request transport failed")),
        }
    }
    pub async fn refresh(&self) -> Result<(), Error> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(SyncCommand::Refresh(reply))
            .map_err(command_error)?;
        result
            .await
            .map_err(|_| Error::unavailable("request transport failed"))?
    }
    pub async fn insert_history(
        &self,
        request: HistoryRequest,
        page: History,
    ) -> Result<HistoryApplied, Error> {
        if request.identity != self.identity {
            return Err(Error::failed_precondition(
                "subscription must be resynchronized",
            ));
        }
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(SyncCommand::History(Box::new(request), page, reply))
            .map_err(command_error)?;
        result
            .await
            .map_err(|_| Error::unavailable("request transport failed"))?
    }
    pub async fn close(mut self) {
        self.stop.send_replace(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}
fn command_error(error: mpsc::error::TrySendError<SyncCommand>) -> Error {
    match error {
        mpsc::error::TrySendError::Full(_) => {
            Error::resource_exhausted("client command queue is full")
        }
        _ => Error::unavailable("request transport failed"),
    }
}
fn history_cursor(snapshot: &Snapshot, companion: &str) -> Option<String> {
    snapshot
        .message_cursors
        .get(companion)
        .and_then(|cursor| cursor.value.clone())
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    http: Http,
    subscription: String,
    mut snapshot: Snapshot,
    mut reader: Reader,
    mut commands: mpsc::Receiver<SyncCommand>,
    notices: mpsc::Sender<Notification>,
    view: Arc<std::sync::Mutex<SessionView>>,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), Error> {
    let mut state = SessionState::from_snapshot(&snapshot);
    let mut generation = 0u64;
    snapshot.messages.clear();
    let mut dirty = false;
    let mut pending_notices = Vec::new();
    let mut frames = tokio::time::interval(Duration::from_millis(33));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if reader.overflow.load(Ordering::Acquire) {
            return Err(Error::failed_precondition(
                "subscription must be resynchronized",
            ));
        }
        tokio::select! {
            biased;
            _ = stopping.changed() => break,
            _ = frames.tick(), if dirty || !pending_notices.is_empty() => {
                publish_view(&view, &snapshot, &state, generation);
                dirty = false;
                for notice in pending_notices.drain(..) {
                    notices.try_send(notice).map_err(|_| Error::failed_precondition("subscription must be resynchronized"))?;
                }
            },
            command = commands.recv() => match command {
                Some(SyncCommand::Refresh(reply)) => {
                    let result = refresh_state(&http, &subscription, &mut snapshot, &mut state).await;
                    if result.is_ok() {
                        generation += 1;
                        publish_view(&view, &snapshot, &state, generation);
                        dirty = false;
                    }
                    let failed = result.clone();
                    let _ = reply.send(result);
                    failed?;
                }
                Some(SyncCommand::History(request, page, reply)) => {
                    let companion = request.request.companion_id;
                    let result = if !snapshot.companions.iter().any(|value| value.id == companion)
                        || page.items.iter().any(|message| message.companion_id != companion) {
                        Err(Error::internal("invalid API response"))
                    } else {
                        let before = state.messages.len();
                        for message in page.items { state.messages.entry(message.id.clone()).or_insert(message); }
                        if generation == request.generation
                            && history_cursor(&snapshot, &companion) == request.request.cursor {
                            snapshot.message_cursors.insert(companion.clone(), HistoryCursor {
                                value: page.next_cursor, ..Default::default()
                            });
                        }
                        publish_view(&view, &snapshot, &state, generation);
                        dirty = false;
                        Ok(HistoryApplied {
                            added_rows: state.messages.len() > before,
                            next_cursor: history_cursor(&snapshot, &companion),
                        })
                    };
                    let _ = reply.send(result);
                }
                None => break,
            },
            event = reader.events.recv() => {
                let event = event.ok_or_else(|| reader.terminal.lock().expect("reader terminal").clone().unwrap_or(Error::unavailable("request transport failed")))?;
                if reader.overflow.load(Ordering::Acquire) { return Err(Error::failed_precondition("subscription must be resynchronized")); }
                let notification = match event.event {
                    Some(host_event::Event::Notification(notification)) => *notification,
                    Some(host_event::Event::ResyncRequired(_)) => return Err(Error::failed_precondition("subscription must be resynchronized")),
                    _ => return Err(Error::internal("invalid API response")),
                };
                if !state.apply(&notification)? { continue; }
                use types::notification::Event;
                match notification.event.as_ref() {
                    Some(Event::SettingsUpdated(_) | Event::CompanionUpdated(_) | Event::GrantsUpdated(_)) => {
                        refresh_state(&http, &subscription, &mut snapshot, &mut state).await?;
                        generation += 1;
                    }
                    Some(Event::HostStorageFailed(_) | Event::GenerationPersistenceFailed(_)) => {
                        let health = snapshot.health.get_or_insert_default();
                        health.ready = false;
                        health.recovery_required = true;
                        for companion in &mut snapshot.companions {
                            let json = &companion.availability.json;
                            let mut availability: serde_json::Value = if json.is_empty() {
                                serde_json::json!({})
                            } else {
                                serde_json::from_str(json).map_err(|_| Error::internal("invalid API response"))?
                            };
                            let object = availability.as_object_mut().ok_or(Error::internal("invalid API response"))?;
                            object.insert("chat".into(), serde_json::json!({"ready":false,"reasons":[{"code":"storage_error"}]}));
                            companion.availability = JsonDocument { json: availability.to_string(), ..Default::default() }.into();
                        }
                    }
                    _ => {}
                }
                dirty = true;
                if pending_notices.len() >= 256 { return Err(Error::failed_precondition("subscription must be resynchronized")); }
                pending_notices.push(notification);
            }
        }
    }
    reader.stop.send_replace(true);
    if let Some(job) = reader.job.take() {
        let _ = job.await;
    }
    Ok(())
}
fn publish_view(
    view: &std::sync::Mutex<SessionView>,
    snapshot: &Snapshot,
    state: &SessionState,
    generation: u64,
) {
    *view.lock().expect("session view") = published_view(snapshot, state, generation);
}
fn published_view(snapshot: &Snapshot, state: &SessionState, generation: u64) -> SessionView {
    let mut messages: Vec<_> = state.messages.values().cloned().collect();
    messages.sort_by_key(|message| message.sequence);
    SessionView {
        generation,
        snapshot: Snapshot {
            cursor: state.cursor.clone().into(),
            messages,
            ..snapshot.clone()
        },
    }
}
async fn refresh_state(
    http: &Http,
    subscription: &str,
    snapshot: &mut Snapshot,
    state: &mut SessionState,
) -> Result<(), Error> {
    let mut fresh = http
        .get_snapshot_with_options(
            ene_rpc::SnapshotRequest {
                subscription_id: Some(subscription.into()),
                ..Default::default()
            },
            call_options(5),
        )
        .await?
        .into_owned();
    if fresh.host_id != snapshot.host_id
        || fresh.cursor.is_unset()
        || fresh.cursor.epoch != state.cursor.epoch
        || fresh.cursor.seq < state.cursor.seq
    {
        return Err(Error::failed_precondition(
            "subscription must be resynchronized",
        ));
    }
    let merged = merge_state(http, &fresh, state).await?;
    fresh.messages.clear();
    *state = merged;
    *snapshot = fresh;
    Ok(())
}

async fn merge_state(
    http: &Http,
    snapshot: &Snapshot,
    state: &SessionState,
) -> Result<SessionState, Error> {
    if snapshot.cursor.is_unset()
        || snapshot.cursor.epoch != state.cursor.epoch
        || snapshot.cursor.seq < state.cursor.seq
    {
        return Err(Error::failed_precondition(
            "subscription must be resynchronized",
        ));
    }
    let mut merged = SessionState::from_snapshot(snapshot);
    for (id, previous) in &state.messages {
        if merged.messages.contains_key(id)
            || !snapshot
                .companions
                .iter()
                .any(|companion| companion.id == previous.companion_id)
        {
            continue;
        }
        let mut message = previous.clone();
        if matches!(message.status.as_str(), "queued" | "streaming") {
            message = http
                .get_message_with_options(
                    ene_rpc::MessageRequest {
                        companion_id: message.companion_id.clone(),
                        message_id: message.id.clone(),
                        ..Default::default()
                    },
                    call_options(5),
                )
                .await?
                .into_owned();
            if message.id != *id || message.companion_id != previous.companion_id {
                return Err(Error::internal("invalid API response"));
            }
            if matches!(message.status.as_str(), "queued" | "streaming") {
                return Err(Error::failed_precondition(
                    "subscription must be resynchronized",
                ));
            }
        }
        merged.messages.insert(id.clone(), message);
    }
    Ok(merged)
}

use tokio::time::timeout;

type RpcStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    ene_rpc::__buffa::view::HostEventView<'static>,
>;

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
