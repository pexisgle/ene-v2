use super::*;
use axum::{
    Json, Router,
    extract::ws::{Message as Frame, WebSocketUpgrade},
    routing::get,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

fn snapshot(seq: u64, text: &str) -> Value {
    json!({"host_id":"h","environment_id":"env","cursor":{"epoch":"e","seq":seq},"capabilities":{},"settings":{"revision":1,"connections":[]},"companions":[{"id":"c","name":"ene","mind":"","revision":1,"body":null,"voice":null,"model_assignments":{},"availability":{}}],"messages":[{"id":"m","companion_id":"c","role":"companion","status":"streaming","text":text,"sequence":2,"created_at":"now","resources":[],"in_reply_to":"u","task_id":null,"question_id":null,"failure":null}],"message_cursors":{"c":null},"tasks":[],"questions":[],"grants":[],"health":{"ready":true,"recovery_required":false,"stopping":false}})
}
struct Fixture {
    client: ApiClient,
    events: mpsc::Sender<Value>,
    snapshots: Arc<Semaphore>,
    release: Arc<Semaphore>,
    closed: Arc<Semaphore>,
    current: Arc<std::sync::Mutex<Value>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(current: Value) -> Fixture {
    let (events, receiver) = mpsc::channel::<Value>(512);
    let receiver = Arc::new(std::sync::Mutex::new(Some(receiver)));
    let snapshots = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let closed = Arc::new(Semaphore::new(0));
    let current = Arc::new(std::sync::Mutex::new(current));
    let ws_receiver = receiver.clone();
    let ws_closed = closed.clone();
    let snap_current = current.clone();
    let snap_started = snapshots.clone();
    let snap_release = release.clone();
    let router = Router::new().route("/v1/events", get(move |ws: WebSocketUpgrade| async move {
        let receiver = ws_receiver.lock().unwrap().take().unwrap(); let closed = ws_closed.clone();
        ws.on_upgrade(move |mut socket| async move {
            let mut receiver = receiver;
            socket.send(Frame::Text(json!({"type":"ready","connection_id":"s","cursor":{"epoch":"e","seq":0}}).to_string().into())).await.unwrap();
            loop {
                tokio::select! {
                    event = receiver.recv() => match event { Some(value) => { if socket.send(Frame::Text(value.to_string().into())).await.is_err() { break; } }, None => break },
                    frame = socket.next() => if matches!(frame, None | Some(Ok(Frame::Close(_))) | Some(Err(_))) { break; },
                }
            }
            closed.add_permits(1);
        })
    })).route("/v1/snapshot", get(move |axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| {
        assert_eq!(query.get("connection_id").map(String::as_str), Some("s"));
        let current = snap_current.clone(); let started = snap_started.clone(); let release = snap_release.clone();
        async move { started.add_permits(1); release.acquire().await.unwrap().forget(); let value = current.lock().unwrap().clone(); Json(value) }
    }));
    let router = router.route(
        "/v1/companions/c/messages",
        get(
            |axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >| async move {
                assert_eq!(query.get("cursor").map(String::as_str), Some("52:c"));
                Json(Page {
                    items: (1..52)
                        .map(|seq| history_message(seq, 0))
                        .collect::<Vec<_>>(),
                    next_cursor: None,
                })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ApiClient::new(&RuntimeConnection {
        endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
        token: "fixture".into(),
    })
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Fixture {
        client,
        events,
        snapshots,
        release,
        closed,
        current,
        server,
    }
}
async fn entered(semaphore: &Semaphore) {
    tokio::time::timeout(Duration::from_secs(3), semaphore.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}
fn delta(seq: u64, offset: usize, text: &str) -> Value {
    json!({"epoch":"e","seq":seq,"type":"message.delta","companion_id":"c","data":{"message_id":"m","offset":offset,"delta":text}})
}
fn settings(seq: u64) -> Value {
    json!({"epoch":"e","seq":seq,"type":"settings.updated","companion_id":null,"data":{}})
}

#[tokio::test]
async fn subscription_buffers_snapshot_overlap_and_cancelled_waiters_cannot_interrupt_refresh() {
    let fixture = fixture(snapshot(1, "日")).await;
    let client = fixture.client.clone();
    let starting = tokio::spawn(async move { client.synced_session().await });
    entered(&fixture.snapshots).await;
    fixture.events.send(delta(1, 999, "covered")).await.unwrap();
    fixture.events.send(delta(2, 1, "🙂")).await.unwrap();
    fixture.release.add_permits(1);
    let mut session = starting.await.unwrap().unwrap();
    assert_eq!(session.next().await.unwrap().seq, 2);
    assert_eq!(session.view().1[0].text, "日🙂");
    let applied = session.snapshot();
    assert_eq!(applied.cursor.seq, 2);
    assert_eq!(applied.messages[0].text, "日🙂");
    let mut replica = Replica::from_snapshot(&applied);
    assert!(
        replica
            .apply(&serde_json::from_value(delta(3, 2, "。")).unwrap())
            .unwrap()
    );
    assert_eq!(replica.messages["m"].text, "日🙂。");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), session.next())
            .await
            .is_err()
    );
    fixture.events.send(delta(3, 2, "。")).await.unwrap();
    assert_eq!(session.next().await.unwrap().seq, 3);
    let mut fresh = snapshot(4, "日🙂。");
    fresh["settings"]["revision"] = json!(2);
    *fixture.current.lock().unwrap() = fresh;
    fixture.events.send(settings(4)).await.unwrap();
    entered(&fixture.snapshots).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), session.next())
            .await
            .is_err()
    );
    fixture.events.send(delta(5, 3, "e\u{301}")).await.unwrap();
    fixture.release.add_permits(1);
    assert_eq!(session.next().await.unwrap().seq, 4);
    assert_eq!(session.snapshot().settings.revision, 2);
    assert_eq!(session.next().await.unwrap().seq, 5);
    assert_eq!(session.view().1[0].text, "日🙂。e\u{301}");
    assert_eq!(session.snapshot().cursor.seq, 5);
    let mut changed = snapshot(6, "日🙂。e\u{301}");
    changed["host_id"] = json!("replacement");
    *fixture.current.lock().unwrap() = changed;
    fixture.events.send(settings(6)).await.unwrap();
    entered(&fixture.snapshots).await;
    fixture.release.add_permits(1);
    assert!(matches!(session.next().await, Err(Error::ResyncRequired)));
    session.close().await;
}
#[tokio::test]
async fn pre_snapshot_notification_overflow_requires_a_new_subscription() {
    let fixture = fixture(snapshot(0, "")).await;
    let client = fixture.client.clone();
    let starting = tokio::spawn(async move { client.synced_session().await });
    entered(&fixture.snapshots).await;
    for seq in 1..=300 {
        fixture
            .events
            .send(
                json!({"epoch":"e","seq":seq,"type":"future.event","companion_id":null,"data":{}}),
            )
            .await
            .unwrap();
    }
    entered(&fixture.closed).await;
    fixture.release.add_permits(1);
    assert!(matches!(
        starting.await.unwrap(),
        Err(Error::ResyncRequired)
    ));
}
#[tokio::test]
async fn sequence_gaps_reject_the_session_without_applying_the_delta() {
    let fixture = fixture(snapshot(0, "日")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    fixture.events.send(delta(2, 1, "bad")).await.unwrap();
    assert!(matches!(session.next().await, Err(Error::ResyncRequired)));
    assert_eq!(session.view().1[0].text, "日");
    session.close().await;
}

fn history_message(sequence: u64, bytes: usize) -> Message {
    serde_json::from_value(json!({"id":format!("history-{sequence}"),"companion_id":"c","role":"user","status":"completed","text":"x".repeat(bytes),"sequence":sequence,"created_at":"now","in_reply_to":null,"task_id":null,"question_id":null,"failure":null})).unwrap()
}
async fn fill_history(session: &SyncedSession) {
    // Each page is below 4 MiB; retained data is just below 64 MiB.
    for sequence in 1..=17 {
        session
            .insert_history(
                "c".into(),
                Page {
                    items: vec![history_message(sequence, 15 * 256 * 1024)],
                    next_cursor: Some(format!("{sequence}:c")),
                },
            )
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn review_history_and_explicit_refresh_cannot_publish_over_budget_state() {
    let fixture = fixture(snapshot(0, "")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    fill_history(&session).await;
    let before = session.snapshot();
    assert!(types::budget::encoded_len(&before.messages, 64 * 1024 * 1024).is_ok());
    assert!(matches!(
        session
            .insert_history(
                "c".into(),
                Page {
                    items: vec![history_message(18, 512 * 1024)],
                    next_cursor: None
                }
            )
            .await,
        Err(Error::ResourceLimit)
    ));
    assert_eq!(session.snapshot(), before);
    let mut fresh = snapshot(1, "");
    fresh["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(history_message(19, 1024 * 1024)).unwrap());
    *fixture.current.lock().unwrap() = fresh;
    fixture.release.add_permits(1);
    assert!(matches!(session.refresh().await, Err(Error::ResourceLimit)));
    assert_eq!(session.snapshot(), before);
    assert!(matches!(session.next().await, Err(Error::ResourceLimit)));
    session.close().await;
}
#[tokio::test]
async fn review_notification_growth_cannot_publish_over_budget_state() {
    let fixture = fixture(snapshot(0, "")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    fill_history(&session).await;
    fixture
        .events
        .send(delta(1, 0, &"x".repeat(512 * 1024)))
        .await
        .unwrap();
    assert!(matches!(session.next().await, Err(Error::ResourceLimit)));
    assert_eq!(session.snapshot().cursor.seq, 0);
    assert!(
        session
            .snapshot()
            .messages
            .iter()
            .find(|m| m.id == "m")
            .unwrap()
            .text
            .is_empty()
    );
    session.close().await;
}
#[tokio::test]
async fn review_host_storage_failure_is_applied_before_delivering_notice() {
    let fixture = fixture(snapshot(0, "before")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    fixture.events.send(json!({"epoch":"e","seq":1,"type":"host.storage_failed","companion_id":null,"data":{"code":"storage_error"}})).await.unwrap();
    assert_eq!(session.next().await.unwrap().kind, "host.storage_failed");
    let state = session.snapshot();
    assert_eq!(state.cursor.seq, 1);
    assert!(!state.health.ready);
    assert!(state.health.recovery_required);
    assert_eq!(state.companions[0].availability["chat"]["ready"], false);
    assert_eq!(
        state.companions[0].availability["chat"]["reasons"][0]["code"],
        "storage_error"
    );
    session.close().await;
}
#[tokio::test]
async fn review_refresh_keeps_gaps_reachable_after_exhausted_history() {
    let mut initial = snapshot(0, "");
    initial["messages"] = serde_json::to_value(vec![history_message(1, 0)]).unwrap();
    let fixture = fixture(initial).await;
    fixture.release.add_permits(1);
    let session = fixture.client.synced_session().await.unwrap();
    session
        .insert_history(
            "c".into(),
            Page {
                items: vec![history_message(1, 0)],
                next_cursor: None,
            },
        )
        .await
        .unwrap();
    let mut fresh = snapshot(1, "");
    fresh["messages"] = serde_json::to_value(
        (52..=101)
            .map(|seq| history_message(seq, 0))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fresh["message_cursors"]["c"] = json!("52:c");
    *fixture.current.lock().unwrap() = fresh;
    fixture.release.add_permits(1);
    session.refresh().await.unwrap();
    let merged = session.snapshot();
    assert_eq!(merged.messages.len(), 51);
    assert_eq!(merged.message_cursors["c"].as_deref(), Some("52:c"));
    let page = fixture
        .client
        .messages("c", merged.message_cursors["c"].as_deref(), None)
        .await
        .unwrap();
    session.insert_history("c".into(), page).await.unwrap();
    let complete = session.snapshot();
    assert_eq!(
        complete
            .messages
            .iter()
            .map(|m| m.sequence)
            .collect::<Vec<_>>(),
        (1..=101).collect::<Vec<_>>()
    );
    assert!(complete.message_cursors["c"].is_none());
    session.close().await;
}
