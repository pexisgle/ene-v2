use super::*;
fn projection(snapshot: &Snapshot, replica: &Replica) -> Event {
    let mut messages: Vec<_> = replica.messages.values().cloned().collect();
    messages.sort_by_key(|m| m.sequence);
    Event::View(Box::new(snapshot.clone()), messages)
}
struct Pending {
    text: String,
    key: String,
    companion: String,
    host: String,
}
pub(super) async fn run(
    commands: channel::UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Sync),
) {
    let path = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(root) => root.join("ene-host/connection.json"),
        Err(_) => {
            emit(Event::Error("XDG_RUNTIME_DIRを利用できません".into()));
            return;
        }
    };
    run_at(commands, emit, &path).await;
}
pub(super) async fn run_at(
    mut commands: channel::UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Sync),
    path: &Path,
) {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(95))
        .build()
        .expect("local HTTP client");
    let mut attempt = 0usize;
    let mut pending: Option<Pending> = None;
    loop {
        emit(Event::Status("ホストに接続しています…".into(), false));
        let result = session(
            &client,
            &mut commands,
            emit,
            &mut pending,
            &mut attempt,
            path,
        )
        .await;
        let reason = match result {
            Ok(()) => return,
            Err(reason) => reason,
        };
        let delay = [1, 2, 4, 8, 15][attempt.min(4)];
        attempt = attempt.saturating_add(1);
        emit(Event::Status(
            format!("{reason} · {delay}秒後に再接続します"),
            false,
        ));
        let wait = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {_= &mut wait=>break, command=commands.recv()=>match command {
                None=>return, Some(Command::Send(_))=>emit(Event::SendFailed("未接続です。接続後に送信してください。".into())),Some(_)=>emit(Event::Error("未接続です。接続後に操作してください。".into()))
            }}
        }
    }
}
async fn session(
    client: &Client,
    commands: &mut channel::UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Sync),
    pending: &mut Option<Pending>,
    attempt: &mut usize,
    path: &Path,
) -> Result<(), String> {
    let connection = std::sync::Arc::new(descriptor(path)?);
    let http = Http {
        client: client.clone(),
        connection: connection.clone(),
    };
    let mut request = format!("{}/events", connection.endpoint.replacen("http:", "ws:", 1))
        .into_client_request()
        .map_err(|_| "通知接続が不正です")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", connection.token)
            .parse()
            .map_err(|_| "接続情報が不正です")?,
    );
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(request))
        .await
        .map_err(|_| "通知接続がタイムアウトしました")?
        .map_err(|_| "通知接続を開始できません")?;
    let ready = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .map_err(|_| "購読応答がありません")?
        .ok_or("購読応答がありません")?
        .map_err(|_| "購読を開始できません")?;
    let ready: Value = serde_json::from_str(ready.to_text().map_err(|_| "購読応答が不正です")?)
        .map_err(|_| "購読応答が不正です")?;
    let id = ready["connection_id"]
        .as_str()
        .filter(|_| ready["type"] == "ready")
        .ok_or("購読応答が不正です")?
        .to_owned();
    let (tx, mut events) = channel::channel(256);
    let reader = tokio::spawn(async move {
        while let Some(frame) = socket.next().await {
            match frame {
                Ok(Frame::Text(text)) => {
                    if tx
                        .send(serde_json::from_str::<Value>(&text).ok())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Frame::Ping(_)) => {
                    if socket.flush().await.is_err() {
                        break;
                    }
                }
                Ok(Frame::Pong(_)) => {}
                _ => break,
            }
        }
        // A received Close already queues its reply; flush by closing the sink.
        let _ = SinkExt::close(&mut socket).await;
    });

    let result = async {
        let mut snapshot = http.snapshot(&id).await.map_err(|_| "スナップショットを取得できません")?;
        let mut replica = Replica::from_snapshot(&snapshot);
        *attempt = 0;
        emit(projection(&snapshot, &replica));
        emit(Event::Status("ホストに接続しました".into(), true));
        if let Ok(value)=http.request(Method::GET,"/providers",None,None).await && let Ok(list)=serde_json::from_value(value){emit(Event::Providers(list));}
        if pending.as_ref().is_some_and(|p| p.host != snapshot.host_id) {
            pending.take();
            emit(Event::SendFailed("接続先ホストが変わったため、未確認の送信を停止しました".into()));
        }
        let mut jobs = tokio::task::JoinSet::new();
        if let Some(p) = pending.as_ref() {
            spawn_send(&mut jobs, &http, p);
        }
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(Some(value)) = event else { return Err("通知接続が切れました".into()); };
                    if value["type"] == "resync_required" {
                        return Err("通知の再同期が必要です".into());
                    }
                    let notification: Notification = serde_json::from_value(value)
                        .map_err(|_| "通知の形式が不正です")?;
                    if !replica.apply(&notification).map_err(|_| "通知の再同期が必要です")? {
                        continue;
                    }
                    if matches!(notification.kind.as_str(), "settings.updated" | "companion.updated" | "grants.updated") {
                        // Buffer deltas while fetching a snapshot on the live subscription.
                        snapshot = http.snapshot(&id).await.map_err(|_| "設定を同期できません")?;
                        if let Ok(value)=http.request(Method::GET,"/providers",None,None).await&&let Ok(list)=serde_json::from_value(value){emit(Event::Providers(list));}
                        replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                    }
                    emit(projection(&snapshot, &replica));
                },
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    match command {
                        Command::Send(text) => {
                            if pending.is_some() {
                                emit(Event::Error("前の発言の受付を確認しています".into()));
                                continue;
                            }
                            let companion = snapshot.companions.first().ok_or("コンパニオンがありません")?.id.clone();
                            *pending = Some(Pending { text, key: Uuid::new_v4().to_string(), companion, host: snapshot.host_id.clone() });
                            spawn_send(&mut jobs, &http, pending.as_ref().unwrap());
                        }
                        command => {
                            let http = http.clone();
                            let current = snapshot.clone();
                            jobs.spawn(async move { Job::Action(action(http, current, command).await) });
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
                        },
                        Some(Ok(Job::Action(Ok(ActionResult::Done(text,committed))))) => {
                            emit(Event::Done(text,committed));
                            if let Ok(value)=http.request(Method::GET,"/providers",None,None).await && let Ok(list)=serde_json::from_value(value){emit(Event::Providers(list));}
                            snapshot = http.snapshot(&id).await.map_err(|_| "設定を同期できません")?;
                            replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                            emit(projection(&snapshot, &replica));
                        }
                        Some(Ok(Job::Action(Ok(ActionResult::Models(list)))))=>emit(Event::Models(list)),
                        Some(Ok(Job::Action(Ok(ActionResult::History(page))))) => {
                            for m in &page.items {
                                replica.messages.entry(m.id.clone()).or_insert_with(|| m.clone());
                            }
                            snapshot.message_cursors.insert(snapshot.companions[0].id.clone(), page.next_cursor.clone());
                            emit(projection(&snapshot, &replica));
                            emit(Event::Older(page));
                        }
                        Some(Ok(Job::Action(Err(Error::Api(text))))) => emit(Event::Error(text)),
                        _ => emit(Event::Error("要求を完了できません。接続を確認してください".into())),
                    }
                },
            }
        }
    }.await;
    reader.abort();
    let _ = reader.await;
    result
}
// Final older rows are immutable. A previously active row omitted by the new
// snapshot has ended outside its latest page; obtain that terminal state first.
pub(super) async fn refreshed(
    http: &Http,
    snapshot: &Snapshot,
    previous: Replica,
) -> Result<Replica, Error> {
    use ene_protocol::host::MessageStatus;
    let mut fresh = Replica::from_snapshot(snapshot);
    for (key, mut message) in previous.messages {
        if fresh.messages.contains_key(&key) {
            continue;
        }
        if matches!(
            message.status,
            MessageStatus::Queued | MessageStatus::Streaming
        ) {
            let path = format!(
                "/companions/{}/messages/{}",
                message.companion_id, message.id
            );
            message = serde_json::from_value(http.request(Method::GET, &path, None, None).await?)
                .map_err(|_| Error::Transport)?;
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
enum Job {
    Sent(Result<Value, Error>),
    Action(Result<ActionResult, Error>),
}
fn spawn_send(jobs: &mut tokio::task::JoinSet<Job>, http: &Http, p: &Pending) {
    let http = http.clone();
    let path = format!("/companions/{}/messages", p.companion);
    let key = p.key.clone();
    let text = p.text.clone();
    jobs.spawn(async move {
        let result = http
            .request(Method::POST, &path, Some(json!({"text":text})), Some(&key))
            .await
            .and_then(|value| {
                serde_json::from_value::<ene_protocol::host::MessageAcceptance>(value.clone())
                    .map_err(|_| Error::Transport)?;
                Ok(value)
            });
        Job::Sent(result)
    });
}
