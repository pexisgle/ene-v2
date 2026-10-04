use super::mutations::{MutationQueue, is_mutation};
use super::*;
use std::collections::BTreeMap;
fn projection(snapshot: &Snapshot, replica: &Replica) -> Event {
    let mut messages: Vec<_> = replica.messages.values().cloned().collect();
    messages.sort_by_key(|m| m.sequence);
    Event::View(Box::new(snapshot.clone()), messages)
}
struct Pending {
    text: String,
    key: IdempotencyKey,
    companion: String,
    host: String,
}
pub(super) async fn run(
    commands: channel::UnboundedReceiver<Request>,
    emit: &(impl Fn(Event) + Sync),
) {
    let path = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(root) => root.join("ene-host/connection.json"),
        Err(_) => {
            emit(Event::Connection(
                ConnectionPhase::Stopped,
                "実行環境を確認できません。デスクトップセッションで起動し直してください。".into(),
            ));
            return;
        }
    };
    run_with_launcher(
        commands,
        emit,
        &path,
        Some(super::launch::Launcher::local()),
    )
    .await;
}
async fn run_with_launcher(
    mut commands: channel::UnboundedReceiver<Request>,
    emit: &(impl Fn(Event) + Sync),
    path: &Path,
    mut launcher: Option<super::launch::Launcher>,
) {
    let mut attempt = 0usize;
    let mut pending: Option<Pending> = None;
    let mut start_requested = true;
    let mutations = MutationQueue::new();
    loop {
        emit(Event::Connection(
            ConnectionPhase::Connecting,
            "このPCのホストに接続しています…".into(),
        ));
        let mut startup_error = None;
        if start_requested {
            start_requested = false;
            if let Some(launcher) = &mut launcher
                && let Err(reason) = launcher.prepare(path, emit).await
            {
                if pending.is_none() {
                    emit(Event::Connection(ConnectionPhase::Stopped, reason));
                    return;
                }
                // Never discard an uncertain send's identity on a failed
                // manual restart. Keep the subscription retry loop alive.
                startup_error = Some(reason);
            }
        }
        if launcher.is_some() && startup_error.is_none() {
            emit(Event::Connection(
                ConnectionPhase::Connecting,
                "ホストに接続して、会話と設定を同期しています…".into(),
            ));
        }
        let result = match startup_error {
            Some(reason) => Err(reason),
            None => {
                session(
                    &mut commands,
                    emit,
                    &mut pending,
                    &mut attempt,
                    path,
                    &mutations,
                )
                .await
            }
        };
        let reason = match result {
            Ok(()) => return,
            Err(reason) => reason,
        };
        let delay = [1, 2, 4, 8, 15][attempt.min(4)];
        attempt = attempt.saturating_add(1);
        emit(Event::RetryIn(delay as i32));
        emit(Event::Connection(ConnectionPhase::Retrying, reason));
        let wait = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {_= &mut wait=>break, command=commands.recv()=>match command {
                None=>return, Some(Request { command: Command::Reconnect, .. })=>{attempt=0;start_requested=true;break;}, Some(Request { command: Command::Send(_), .. })=>emit(Event::SendFailed("未接続です。接続後に送信してください。".into())),Some(_)=>emit(Event::Error("未接続です。接続後に操作してください。".into()))
            }}
        }
    }
}
async fn session(
    commands: &mut channel::UnboundedReceiver<Request>,
    emit: &(impl Fn(Event) + Sync),
    pending: &mut Option<Pending>,
    attempt: &mut usize,
    path: &Path,
    mutations: &MutationQueue,
) -> Result<(), String> {
    let http = Http::new(&descriptor(path)?).map_err(|_| "接続情報が不正です")?;
    let mut socket = http
        .subscribe()
        .await
        .map_err(|_| "通知接続を開始できません")?;
    let id = socket.ready().connection_id.clone();
    let (tx, mut events) = channel::channel(256);
    let (stop, mut stopping) = tokio::sync::watch::channel(false);
    let reader = tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                biased; _ = stopping.changed() => break,
                frame = socket.next() => frame,
            };
            match frame {
                Ok(Some(event)) => {
                    let sent = tokio::select! {
                        biased; _ = stopping.changed() => break,
                        sent = tx.send(event) => sent,
                    };
                    if sent.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = socket.close().await;
    });

    let result = async {
        let mut snapshot = http.snapshot(Some(&id)).await.map_err(|_| "スナップショットを取得できません")?;
        let mut replica = Replica::from_snapshot(&snapshot);
        let mut history_cursors = BTreeMap::new();
        *attempt = 0;
        emit(projection(&snapshot, &replica));
        emit(Event::Connection(ConnectionPhase::Connected, "このPCのホストに接続しました。".into()));
        if let Ok(list)=http.providers().await {emit(Event::Providers(list));}
        if pending.as_ref().is_some_and(|p| p.host != snapshot.host_id) {
            pending.take();
            emit(Event::SendFailed("接続先ホストが変わったため、未確認の送信を停止しました".into()));
        }
        let mut jobs = tokio::task::JoinSet::new();
        if let Some(p) = pending.as_ref() {
            spawn_send(&mut jobs, &http, p);
        }
        let mut dirty = false;
        let mut frames = tokio::time::interval(Duration::from_millis(33));
        frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = frames.tick(), if dirty => {
                    emit(projection(&snapshot, &replica));
                    dirty = false;
                },
                event = events.recv() => {
                    let notification = match event {
                        Some(ServerEvent::Notification(value)) => value,
                        Some(ServerEvent::ResyncRequired) => return Err("通知の再同期が必要です".into()),
                        None => return Err("通知接続が切れました".into()),
                    };
                    if !replica.apply(&notification).map_err(|_| "通知の再同期が必要です")? {
                        continue;
                    }
                    if matches!(notification.kind.as_str(), "settings.updated" | "companion.updated" | "grants.updated") {
                        // Buffer deltas while fetching a snapshot on the live subscription.
                        snapshot = refresh_snapshot(&http, &id, &history_cursors).await.map_err(|_| "設定を同期できません")?;
                        if let Ok(list)=http.providers().await {emit(Event::Providers(list));}
                        replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                    }
                    if notification.kind == "generation.persistence_failed" {
                        if let Some(companion) = snapshot.companions.iter_mut().find(|c| Some(&c.id) == notification.companion_id.as_ref()) {
                            companion.availability["chat"] = serde_json::json!({"ready":false,"reasons":[{"code":"storage_error"}]});
                        }
                        emit(Event::Error("生成の終了状態を保存できません。ホストの保存先を確認してください".into()));
                    }
                    dirty = true;
                },
                command = commands.recv() => {
                    let Some(Request { companion, command, context }) = command else { return Ok(()); };
                    match command {
                        Command::Reconnect => {},
                        Command::Send(text) => {
                            if pending.is_some() {
                                emit(Event::Error("前の発言の受付を確認しています".into()));
                                continue;
                            }
                            if !snapshot.companions.iter().any(|c| c.id == companion) {
                                emit(Event::SendFailed("会話相手が見つかりません。選び直してください。".into()));
                                continue;
                            }
                            *pending = Some(Pending { text, key: IdempotencyKey::new(Uuid::new_v4().to_string()).expect("UUID key"), companion, host: snapshot.host_id.clone() });
                            spawn_send(&mut jobs, &http, pending.as_ref().unwrap());
                        }
                        command => {
                            spawn_action(&mut jobs, mutations, http.clone(), snapshot.clone(), companion, command, context);
                        }
                    }
                },
                job = jobs.join_next(), if !jobs.is_empty() => {
                    match job {
                        Some(Ok(Job::Sent(result))) => match result {
                            Ok(_) => {
                                if let Some(p) = pending.take() { emit(Event::Accepted(p.text)); }
                            }
                            Err(Error::Transport | Error::Unauthorized) => {
                                return Err("送信の受付を確認できません。同じキーで再確認します".into());
                            }
                            Err(Error::Api(error)) => {
                                pending.take();
                                emit(Event::SendFailed(error));
                            }
                            Err(Error::PartialConfiguration { .. }) => unreachable!("send cannot partially configure"),
                        },
                        Some(Ok(Job::Action(request, Ok(ActionResult::Done(text,committed,connection))))) => {
                            emit_action(emit, request, Event::Done(text,committed,connection));
                            if let Ok(list)=http.providers().await {emit(Event::Providers(list));}
                            snapshot = refresh_snapshot(&http, &id, &history_cursors).await.map_err(|_| "設定を同期できません")?;
                            replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                            emit(projection(&snapshot, &replica));
                        }
                        Some(Ok(Job::Action(request, Ok(ActionResult::Models(id,list)))))=>emit_action(emit, request, Event::Models(id,list)),
                        Some(Ok(Job::Action(request, Ok(ActionResult::History(companion, page))))) => {
                            for m in &page.items {
                                replica.messages.entry(m.id.clone()).or_insert_with(|| m.clone());
                            }
                            history_cursors.insert(companion.clone(), page.next_cursor.clone());
                            snapshot.message_cursors.insert(companion, page.next_cursor.clone());
                            emit_action(emit, request, Event::Older(page));
                            emit(projection(&snapshot, &replica));
                        }
                        Some(Ok(Job::Action(request, Err(error)))) => {
                            let error = if let Error::PartialConfiguration { connection, error } = error {
                                emit_action(emit, request.clone(), Event::ConnectionSaved(connection));
                                snapshot = refresh_snapshot(&http, &id, &history_cursors).await.map_err(|_| "設定を同期できません")?;
                                replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                                emit(projection(&snapshot, &replica));
                                *error
                            } else { error };
                            emit_action(emit, request, Event::Error(match error {
                                Error::Api(text) => text,
                                _ => "要求を完了できません。接続を確認してください".into(),
                            }));
                        },
                        _ => emit(Event::Error("要求を完了できません。接続を確認してください".into())),
                    }
                },
            }
        }
    }.await;
    stop.send_replace(true);
    let _ = reader.await;
    result
}
pub(super) async fn refresh_snapshot(
    http: &Http,
    id: &str,
    history_cursors: &BTreeMap<String, Option<String>>,
) -> Result<Snapshot, Error> {
    let mut snapshot = http.snapshot(Some(id)).await?;
    for (companion, cursor) in history_cursors {
        if snapshot.companions.iter().any(|c| &c.id == companion) {
            snapshot
                .message_cursors
                .insert(companion.clone(), cursor.clone());
        }
    }
    Ok(snapshot)
}
// Final older rows are immutable. A previously active row omitted by the new
// snapshot has ended outside its latest page; obtain that terminal state first.
pub(super) async fn refreshed(
    http: &Http,
    snapshot: &Snapshot,
    previous: Replica,
) -> Result<Replica, Error> {
    use ene_api_types::host::MessageStatus;
    let mut fresh = Replica::from_snapshot(snapshot);
    for (key, mut message) in previous.messages {
        if fresh.messages.contains_key(&key) {
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
                return Err(Error::Transport);
            }
        }
        fresh.messages.insert(key, message);
    }
    Ok(fresh)
}
fn emit_action(emit: &(impl Fn(Event) + Sync), request: Option<RequestContext>, event: Event) {
    emit(if let Some(request) = request {
        Event::Action(request, Box::new(event))
    } else {
        event
    });
}
pub(super) enum Job {
    Sent(Result<ene_api_types::MessageAcceptance, Error>),
    Action(Option<RequestContext>, Result<ActionResult, Error>),
}
pub(super) fn spawn_action(
    jobs: &mut tokio::task::JoinSet<Job>,
    mutations: &MutationQueue,
    http: Http,
    snapshot: Snapshot,
    companion: String,
    command: Command,
    context: Option<RequestContext>,
) {
    if is_mutation(&command) {
        let result = mutations.submit(http, companion, command);
        jobs.spawn(
            async move { Job::Action(context, result.await.unwrap_or(Err(Error::Transport))) },
        );
    } else {
        jobs.spawn(async move {
            Job::Action(context, action(http, snapshot, companion, command).await)
        });
    }
}
fn spawn_send(jobs: &mut tokio::task::JoinSet<Job>, http: &Http, p: &Pending) {
    let http = http.clone();
    let companion = p.companion.clone();
    let key = p.key.clone();
    let text = p.text.clone();
    jobs.spawn(async move {
        let result = http
            .send_message(
                &companion,
                &ene_api_types::MessageInput {
                    text,
                    resources: vec![],
                    question_id: None,
                    decision: None,
                },
                &key,
            )
            .await
            .map_err(Error::from);
        Job::Sent(result)
    });
}
