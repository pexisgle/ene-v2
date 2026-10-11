use super::mutations::{MutationQueue, is_mutation};
use super::*;
fn projection(session: &ene_api_client::SyncedSession) -> Event {
    let mut snapshot = session.view();
    let messages = std::mem::take(&mut snapshot.messages);
    Event::View(Box::new(snapshot), messages)
}
struct SessionFailure {
    reason: String,
    retry: bool,
}
impl From<String> for SessionFailure {
    fn from(reason: String) -> Self {
        Self {
            reason,
            retry: true,
        }
    }
}
impl From<&str> for SessionFailure {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}
impl From<ene_api_client::Error> for SessionFailure {
    fn from(error: ene_api_client::Error) -> Self {
        let retry = matches!(
            error,
            ene_api_client::Error::Transport
                | ene_api_client::Error::Unauthorized
                | ene_api_client::Error::ResyncRequired
        );
        let reason = match error {
            ene_api_client::Error::ResourceLimit => {
                "会話の読み込み量が上限を超えました。再接続して必要な履歴だけを読み込んでください。"
                    .into()
            }
            ene_api_client::Error::Api(error) if error.body.error.code == "resource_limit" => {
                "ホストの読み込み上限を超えています。保存内容を確認してから再接続してください。"
                    .into()
            }
            error => error.to_string(),
        };
        Self { reason, retry }
    }
}
struct Pending {
    text: String,
    key: IdempotencyKey,
    companion: String,
    host: String,
}
pub(super) async fn run(commands: channel::Receiver<Request>, emit: &(impl Fn(Event) + Sync)) {
    let path = match ene_platform::paths::runtime() {
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
    mut commands: channel::Receiver<Request>,
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
            Some(reason) => Err(SessionFailure::from(reason)),
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
            Err(failure) => {
                if !failure.retry {
                    if pending.take().is_some() {
                        emit(Event::SendFailed(failure.reason.clone()));
                    }
                    emit(Event::Connection(ConnectionPhase::Stopped, failure.reason));
                    return;
                }
                failure.reason
            }
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
    commands: &mut channel::Receiver<Request>,
    emit: &(impl Fn(Event) + Sync),
    pending: &mut Option<Pending>,
    attempt: &mut usize,
    path: &Path,
    mutations: &MutationQueue,
) -> Result<(), SessionFailure> {
    let http = Http::new(&descriptor(path).map_err(SessionFailure::from)?)
        .map_err(SessionFailure::from)?;
    let mut synced = http.synced_session().await.map_err(SessionFailure::from)?;
    let result = async {
        *attempt = 0;
        emit(Event::Session(synced.identity().clone()));
        emit(projection(&synced));
        emit(Event::Connection(ConnectionPhase::Connected, "このPCのホストに接続しました。".into()));
        if let Ok(list)=http.providers().await {emit(Event::Providers(list));}
        if pending.as_ref().is_some_and(|p| p.host != synced.snapshot().host_id) {
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
                    emit(projection(&synced));
                    dirty = false;
                },
                event = synced.next() => {
                    let notification = event.map_err(SessionFailure::from)?;
                    if matches!(notification.kind.as_str(), "settings.updated" | "companion.updated" | "grants.updated") && let Ok(list)=http.providers().await {emit(Event::Providers(list));}
                    if notification.kind == "host.storage_failed" {
                        emit(Event::Error("保存に失敗したためホストを実行できません。ホストの保存先を確認してください".into()));
                    } else if notification.kind == "generation.persistence_failed" {
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
                            if !synced.snapshot().companions.iter().any(|c| c.id == companion) {
                                emit(Event::SendFailed("会話相手が見つかりません。選び直してください。".into()));
                                continue;
                            }
                            *pending = Some(Pending { text, key: IdempotencyKey::new(Uuid::new_v4().to_string()).expect("UUID key"), companion, host: synced.snapshot().host_id.clone() });
                            spawn_send(&mut jobs, &http, pending.as_ref().unwrap());
                        }
                        Command::History => {
                            let Some(context) = context else { continue; };
                            let request = synced.history_request(&companion);
                            if request.expected_before.is_none() {
                                emit_action(emit, context, ActionEvent::Done("過去の履歴はすべて表示しています".into(), false, None));
                                continue;
                            }
                            let http = http.clone();
                            jobs.spawn(async move {
                                let result = http.messages(&request.companion, request.expected_before.as_deref(), None).await.map_err(Error::from);
                                Job::History(Box::new(context), request, result)
                            });
                        }
                        command => {
                            let Some(context) = context else { emit(Event::Error("操作の対象を確認できません。再接続してください".into())); continue; };
                            spawn_action(&mut jobs, mutations, http.clone(), synced.snapshot(), companion, command, context);
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
                            emit_action(emit, *request, ActionEvent::Done(text,committed,connection));
                            if let Ok(list)=http.providers().await {emit(Event::Providers(list));}
                            synced.refresh().await.map_err(SessionFailure::from)?;
                                emit(projection(&synced));
                        }
                        Some(Ok(Job::Action(request, Ok(ActionResult::Models(id,list)))))=>emit_action(emit, *request, ActionEvent::Models(id,list)),
                        Some(Ok(Job::Action(request, Ok(ActionResult::Availability(value))))) => emit_action(emit, *request, ActionEvent::Availability(value)),
                        Some(Ok(Job::History(request, history, Ok(page)))) => {
                            let applied = synced.insert_history(history, page).await.map_err(SessionFailure::from)?;
                            emit_action(emit, *request, ActionEvent::Older(applied));
                            emit(projection(&synced));
                        }
                        Some(Ok(Job::History(request, _, Err(error)))) |
                        Some(Ok(Job::Action(request, Err(error)))) => {
                            let error = if let Error::PartialConfiguration { connection, error } = error {
                                emit_action(emit, *request.clone(), ActionEvent::ConnectionSaved(connection));
                                synced.refresh().await.map_err(SessionFailure::from)?;
                                        emit(projection(&synced));
                                *error
                            } else { error };
                            emit_action(emit, *request, ActionEvent::Error(match error {
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
    synced.close().await;
    result
}
#[cfg(test)]
pub(super) use ene_api_client::session::merge_replica as refreshed;
fn emit_action(emit: &(impl Fn(Event) + Sync), context: RequestContext, payload: ActionEvent) {
    emit(Event::Action(Box::new(ActionResultEnvelope {
        context,
        payload,
    })));
}
pub(super) enum Job {
    Sent(Result<ene_api_types::MessageAcceptance, Error>),
    Action(Box<RequestContext>, Result<ActionResult, Error>),
    History(
        Box<RequestContext>,
        ene_api_client::session::HistoryRequest,
        Result<ene_api_types::host::Page<ene_api_types::host::Message>, Error>,
    ),
}
pub(super) fn spawn_action(
    jobs: &mut tokio::task::JoinSet<Job>,
    mutations: &MutationQueue,
    http: Http,
    snapshot: Snapshot,
    companion: String,
    command: Command,
    context: RequestContext,
) {
    if is_mutation(&command) {
        let key = IdempotencyKey::new(context.id.clone()).expect("controller request key");
        let result = mutations.submit(
            http,
            companion,
            command,
            key,
            context.session.host_id.clone(),
        );
        jobs.spawn(async move {
            Job::Action(
                Box::new(context),
                result.await.unwrap_or(Err(Error::Transport)),
            )
        });
    } else {
        jobs.spawn(async move {
            let key = IdempotencyKey::new(context.id.clone()).expect("controller request key");
            Job::Action(
                Box::new(context),
                action_with_key(http, snapshot, companion, command, key).await,
            )
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
