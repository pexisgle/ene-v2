//! One subscription owns buffering, snapshots, event ordering and history reconciliation.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{mpsc, watch};

const MAX_REPLICA_BYTES: usize = 64 * 1024 * 1024;
struct BufferedEvent {
    event: ServerEvent,
    bytes: usize,
    budget: Arc<AtomicUsize>,
}
impl Drop for BufferedEvent {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Reader {
    stop: watch::Sender<bool>,
    job: Option<tokio::task::JoinHandle<()>>,
    events: mpsc::Receiver<BufferedEvent>,
    overflow: Arc<AtomicBool>,
    terminal: Arc<std::sync::Mutex<Option<Error>>>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl Reader {
    fn start(mut stream: EventStream) -> Self {
        let (sender, events) = mpsc::channel(256);
        let (stop, mut stopping) = watch::channel(false);
        let overflow = Arc::new(AtomicBool::new(false));
        let exceeded = overflow.clone();
        let terminal = Arc::new(std::sync::Mutex::new(None));
        let terminal_error = terminal.clone();
        let budget = Arc::new(AtomicUsize::new(0));
        let job = tokio::spawn(async move {
            loop {
                let event = tokio::select! { biased; _ = stopping.changed() => break, event = stream.next() => event };
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {
                        *terminal_error.lock().expect("reader terminal") = Some(error);
                        break;
                    }
                };
                let bytes = match &event {
                    ServerEvent::Notification(notification) => {
                        types::budget::encoded_len(notification, MAX_RESPONSE)
                            .unwrap_or(MAX_RESPONSE + 1)
                    }
                    ServerEvent::ResyncRequired => 1,
                };
                if budget
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        current
                            .checked_add(bytes)
                            .filter(|next| *next <= MAX_RESPONSE)
                    })
                    .is_err()
                {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
                let buffered = BufferedEvent {
                    event,
                    bytes,
                    budget: budget.clone(),
                };
                if sender.try_send(buffered).is_err() {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
            }
            let _ = stream.close().await;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub host_id: String,
    pub epoch: String,
    pub subscription: String,
}
struct SessionView {
    snapshot: Snapshot,
}
struct Notice {
    notification: Notification,
    bytes: usize,
    budget: Arc<AtomicUsize>,
}
impl Drop for Notice {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
enum SyncCommand {
    Refresh(tokio::sync::oneshot::Sender<Result<(), Error>>),
    History(
        String,
        Page<Message>,
        tokio::sync::oneshot::Sender<Result<(), Error>>,
    ),
}

/// The worker owns application of events. Cancelling a waiter cannot consume half an update.
pub struct SyncedSession {
    identity: SessionIdentity,
    view: Arc<std::sync::Mutex<SessionView>>,
    commands: mpsc::Sender<SyncCommand>,
    notices: mpsc::Receiver<Notice>,
    failure: watch::Receiver<Option<Error>>,
    stop: watch::Sender<bool>,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for SyncedSession {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl ApiClient {
    pub async fn synced_session(&self) -> Result<SyncedSession, Error> {
        let stream = self.subscribe().await?;
        let subscription = stream.ready().connection_id.clone();
        let epoch = stream.ready().cursor.epoch.clone();
        let reader = Reader::start(stream);
        let snapshot = self.snapshot(Some(&subscription)).await?;
        if snapshot.cursor.epoch != epoch || reader.overflow.load(Ordering::Acquire) {
            return Err(Error::ResyncRequired);
        }
        let identity = SessionIdentity {
            host_id: snapshot.host_id.clone(),
            epoch,
            subscription: subscription.clone(),
        };
        let view = Arc::new(std::sync::Mutex::new(published_view(
            &snapshot,
            &Replica::from_snapshot(&snapshot),
        )?));
        let (commands, receiver) = mpsc::channel(64);
        let (notices, incoming) = mpsc::channel(256);
        let (failure, failed) = watch::channel(None);
        let (stop, stopping) = watch::channel(false);
        let worker_view = view.clone();
        let http = self.clone();
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
    /// This value can safely seed `Replica::from_snapshot`.
    pub fn snapshot(&self) -> Snapshot {
        self.view.lock().expect("session view").snapshot.clone()
    }
    pub fn view(&self) -> (Snapshot, Vec<Message>) {
        let view = self.view.lock().expect("session view");
        (view.snapshot.clone(), view.snapshot.messages.clone())
    }
    pub async fn next(&mut self) -> Result<Notification, Error> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        let notice = tokio::select! {
            biased;
            changed = self.failure.changed() => {
                let _ = changed;
                return Err(self.failure.borrow().clone().unwrap_or(Error::Transport));
            }
            notice = self.notices.recv() => notice.ok_or(Error::Transport)?,
        };
        Ok(notice.notification.clone())
    }
    pub async fn refresh(&self) -> Result<(), Error> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(SyncCommand::Refresh(reply))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                _ => Error::Transport,
            })?;
        result.await.map_err(|_| Error::Transport)?
    }
    pub async fn insert_history(
        &self,
        companion: String,
        page: Page<Message>,
    ) -> Result<(), Error> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(SyncCommand::History(companion, page, reply))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                _ => Error::Transport,
            })?;
        result.await.map_err(|_| Error::Transport)?
    }
    pub async fn close(mut self) {
        self.stop.send_replace(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    http: ApiClient,
    subscription: String,
    mut snapshot: Snapshot,
    mut reader: Reader,
    mut commands: mpsc::Receiver<SyncCommand>,
    notices: mpsc::Sender<Notice>,
    view: Arc<std::sync::Mutex<SessionView>>,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), Error> {
    let mut replica = Replica::from_snapshot(&snapshot);
    // Only Replica owns mutable message data; snapshot retains metadata.
    snapshot.messages.clear();
    let budget = Arc::new(AtomicUsize::new(0));
    let mut dirty = false;
    let mut pending_notices = Vec::new();
    let mut frames = tokio::time::interval(Duration::from_millis(33));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if reader.overflow.load(Ordering::Acquire) {
            return Err(Error::ResyncRequired);
        }
        tokio::select! {
            biased;
            _ = stopping.changed() => break,
            _ = frames.tick(), if dirty || !pending_notices.is_empty() => {
                publish_view(&view, &snapshot, &replica)?; dirty = false;
                for notice in pending_notices.drain(..) { notices.try_send(notice).map_err(|_| Error::ResyncRequired)?; }
            },
            command = commands.recv() => match command {
                Some(SyncCommand::Refresh(reply)) => {
                    let result = match refresh_state(&http, &subscription, &mut snapshot, &mut replica).await {
                        Ok(()) => publish_view(&view, &snapshot, &replica),
                        Err(error) => Err(error),
                    };
                    if result.is_ok() { dirty = false; }
                    let failed = result.clone();
                    let _ = reply.send(result);
                    failed?;
                }
                Some(SyncCommand::History(companion, page, reply)) => {
                    let result = if !snapshot.companions.iter().any(|value| value.id == companion)
                        || page.items.iter().any(|message| message.companion_id != companion) { Err(Error::Protocol) }
                    else if types::budget::encoded_len(&page, types::budget::MAX_HISTORY_PAGE_BYTES).is_err() { Err(Error::ResourceLimit) }
                    else {
                        let mut candidate = Replica { cursor: replica.cursor.clone(), messages: replica.messages.clone() };
                        for message in page.items { candidate.messages.entry(message.id.clone()).or_insert(message); }
                        let mut metadata = snapshot.clone();
                        metadata.message_cursors.insert(companion, page.next_cursor);
                        match publish_view(&view, &metadata, &candidate) {
                            Ok(()) => { replica = candidate; snapshot = metadata; dirty = false; Ok(()) }
                            Err(error) => Err(error),
                        }
                    };
                    let _ = reply.send(result);
                }
                None => break,
            },
            buffered = reader.events.recv() => {
                let buffered = buffered.ok_or_else(|| reader.terminal.lock().expect("reader terminal").clone().unwrap_or(Error::Transport))?;
                if reader.overflow.load(Ordering::Acquire) { return Err(Error::ResyncRequired); }
                let notification = match &buffered.event {
                    ServerEvent::Notification(notification) => notification.clone(),
                    ServerEvent::ResyncRequired => return Err(Error::ResyncRequired),
                };
                if !replica.apply(&notification).map_err(|_| Error::ResyncRequired)? { continue; }
                if matches!(notification.kind.as_str(), "settings.updated" | "companion.updated" | "grants.updated") {
                    refresh_state(&http, &subscription, &mut snapshot, &mut replica).await?;
                }
                if matches!(notification.kind.as_str(), "host.storage_failed" | "generation.persistence_failed") {
                    snapshot.health.ready = false;
                    snapshot.health.recovery_required = true;
                    for companion in &mut snapshot.companions { companion.availability["chat"] = serde_json::json!({"ready":false,"reasons":[{"code":"storage_error"}]}); }
                }
                dirty = true;
                let bytes = types::budget::encoded_len(&notification, MAX_RESPONSE).map_err(|_| Error::ResyncRequired)?;
                budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| current.checked_add(bytes).filter(|next| *next <= MAX_RESPONSE)).map_err(|_| Error::ResyncRequired)?;
                if pending_notices.len() >= 256 { budget.fetch_sub(bytes, Ordering::AcqRel); return Err(Error::ResyncRequired); }
                pending_notices.push(Notice { notification, bytes, budget: budget.clone() });
            }
        }
    }
    reader.stop.send_replace(true);
    if let Some(job) = reader.job.take() {
        let _ = job.await;
    }
    Ok(())
}
fn validate_replica(replica: &Replica) -> Result<(), Error> {
    types::budget::encoded_len(&replica.messages, MAX_REPLICA_BYTES)
        .map(|_| ())
        .map_err(|_| Error::ResourceLimit)
}
// Every public state is validated here, including refresh and history commands.
fn publish_view(
    view: &std::sync::Mutex<SessionView>,
    snapshot: &Snapshot,
    replica: &Replica,
) -> Result<(), Error> {
    let published = published_view(snapshot, replica)?;
    *view.lock().expect("session view") = published;
    Ok(())
}
fn published_view(snapshot: &Snapshot, replica: &Replica) -> Result<SessionView, Error> {
    validate_replica(replica)?;
    let mut messages: Vec<_> = replica.messages.values().cloned().collect();
    messages.sort_by_key(|message| message.sequence);
    Ok(SessionView {
        snapshot: Snapshot {
            cursor: replica.cursor.clone(),
            messages,
            ..snapshot.clone()
        },
    })
}
async fn refresh_state(
    http: &ApiClient,
    subscription: &str,
    snapshot: &mut Snapshot,
    replica: &mut Replica,
) -> Result<(), Error> {
    let mut fresh = http.snapshot(Some(subscription)).await?;
    if fresh.host_id != snapshot.host_id || fresh.cursor.epoch != snapshot.cursor.epoch {
        return Err(Error::ResyncRequired);
    }
    let previous = Replica {
        cursor: replica.cursor.clone(),
        messages: replica.messages.clone(),
    };
    let merged = merge_replica(http, &fresh, previous).await?;
    fresh.messages.clear();
    *replica = merged;
    *snapshot = fresh;
    Ok(())
}

#[doc(hidden)]
pub async fn merge_replica(
    http: &ApiClient,
    snapshot: &Snapshot,
    previous: Replica,
) -> Result<Replica, Error> {
    if previous.cursor.epoch != snapshot.cursor.epoch {
        return Err(Error::ResyncRequired);
    }
    let mut fresh = Replica::from_snapshot(snapshot);
    for (id, mut message) in previous.messages {
        if fresh.messages.contains_key(&id)
            || !snapshot
                .companions
                .iter()
                .any(|companion| companion.id == message.companion_id)
        {
            continue;
        }
        if matches!(
            message.status,
            MessageStatus::Queued | MessageStatus::Streaming
        ) {
            message = http.message(&message.companion_id, &message.id).await?;
            if matches!(
                message.status,
                MessageStatus::Queued | MessageStatus::Streaming
            ) {
                return Err(Error::ResyncRequired);
            }
        }
        fresh.messages.insert(id, message);
    }
    validate_replica(&fresh)?;
    Ok(fresh)
}
