use super::*;
use axum::{Json, Router, routing::post};
use buffa::Message as _;
use ene_api_types::{rpc as w, rpc_conversion::RpcConvert};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod connect_fixture;

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
    let initial_current = current.clone();
    let initial_started = snapshots.clone();
    let initial_release = release.clone();
    let router = Router::new().route("/rpc/ene.host.v1.HostService/Subscribe", post(move || {
        let receiver = ws_receiver.lock().unwrap().take().unwrap();
        let closed = ws_closed.clone();
        let current = initial_current.clone();
        let started = initial_started.clone();
        let release = initial_release.clone();
        async move {
            let stream = async_stream::stream! {
                struct Closed(Arc<Semaphore>);
                impl Drop for Closed { fn drop(&mut self) {self.0.add_permits(1);} }
                let _closed = Closed(closed);
                started.add_permits(1);
                release.acquire().await.unwrap().forget();
                let snapshot: Snapshot = serde_json::from_value(current.lock().unwrap().clone()).unwrap();
                let initial = w::HostEvent {event:Some(w::host_event::Event::Initial(Box::new(w::InitialState {subscription_id:"s".into(),snapshot:snapshot.into_wire().unwrap().into(),..Default::default()}))),..Default::default()};
                yield Ok::<_, std::convert::Infallible>(frame(initial));
                let mut receiver = receiver;
                while let Some(value) = receiver.recv().await {
                    let notification: Notification = serde_json::from_value(value).unwrap();
                    let event = w::HostEvent {event:Some(w::host_event::Event::Notification(Box::new(notification.into_wire().unwrap()))),..Default::default()};
                    yield Ok(frame(event));
                }
                yield Ok(vec![2,0,0,0,2,b'{',b'}']);
            };
            axum::response::Response::builder().header("content-type","application/connect+proto").body(axum::body::Body::from_stream(stream)).unwrap()
        }
    })).merge(connect_fixture::router(Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move |axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| {
            assert_eq!(query.get("connection_id").map(String::as_str), Some("s"));
            let current = snap_current.clone(); let started = snap_started.clone(); let release = snap_release.clone();
            async move { started.add_permits(1); release.acquire().await.unwrap().forget(); Json(current.lock().unwrap().clone()) }
        }))
        .route("/rpc/ene.host.v1.HostService/GetHistory", post(|axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            assert_eq!(query.get("cursor").map(String::as_str),Some("52:c"));
            Json(Page {items:(1..52).map(|seq|history_message(seq,0)).collect::<Vec<_>>(),next_cursor:None})
        }))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ApiClient::new(&RuntimeConnection {
        endpoint: format!("http://{}/rpc", listener.local_addr().unwrap()),
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
    json!({"epoch":"e","seq":seq,"type":"settings.updated","companion_id":null,"data":{"revision":1,"connections":[]}})
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
    assert_eq!(session.view().messages[0].text, "日🙂");
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
    assert_eq!(session.view().messages[0].text, "日🙂。e\u{301}");
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
async fn notification_overflow_during_refresh_requires_a_new_subscription() {
    let fixture = fixture(snapshot(0, "")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    entered(&fixture.snapshots).await;
    fixture.events.send(settings(1)).await.unwrap();
    entered(&fixture.snapshots).await;
    for seq in 2..=301 {
        fixture.events.send(json!({"epoch":"e","seq":seq,"type":"message.delta","companion_id":"c","data":{"message_id":"m","offset":seq-2,"delta":"x"}})).await.unwrap();
    }
    entered(&fixture.closed).await;
    fixture.release.add_permits(1);
    assert!(matches!(session.next().await, Err(Error::ResyncRequired)));
    session.close().await;
}
#[tokio::test]
async fn sequence_gaps_reject_the_session_without_applying_the_delta() {
    let fixture = fixture(snapshot(0, "日")).await;
    fixture.release.add_permits(1);
    let mut session = fixture.client.synced_session().await.unwrap();
    fixture.events.send(delta(2, 1, "bad")).await.unwrap();
    assert!(matches!(session.next().await, Err(Error::ResyncRequired)));
    assert_eq!(session.view().messages[0].text, "日");
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
                session.history_request("c"),
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
                session.history_request("c"),
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
            session.history_request("c"),
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
    session
        .insert_history(session.history_request("c"), page)
        .await
        .unwrap();
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

fn frame(message: w::HostEvent) -> Vec<u8> {
    let payload = message.encode_to_vec();
    let mut bytes = vec![0];
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes
}

#[tokio::test]
async fn delayed_history_after_refresh_preserves_the_new_frontier() {
    let mut initial = snapshot(0, "");
    initial["messages"] = serde_json::to_value(
        (101..=150)
            .map(|s| history_message(s, 0))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    initial["message_cursors"]["c"] = json!("101:c");
    let fixture = fixture(initial).await;
    fixture.release.add_permits(1);
    let session = fixture.client.synced_session().await.unwrap();
    let old = session.history_request("c");
    let mut fresh = snapshot(1, "");
    fresh["messages"] = serde_json::to_value(
        (201..=250)
            .map(|s| history_message(s, 0))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fresh["message_cursors"]["c"] = json!("201:c");
    *fixture.current.lock().unwrap() = fresh;
    fixture.release.add_permits(1);
    session.refresh().await.unwrap();
    let applied = session
        .insert_history(
            old,
            Page {
                items: (51..=100).map(|s| history_message(s, 0)).collect(),
                next_cursor: Some("51:c".into()),
            },
        )
        .await
        .unwrap();
    assert!(applied.added_rows);
    assert_eq!(applied.next_cursor.as_deref(), Some("201:c"));
    assert_eq!(
        session.snapshot().message_cursors["c"].as_deref(),
        Some("201:c")
    );
    assert!(session.snapshot().messages.iter().any(|m| m.sequence == 51));
    let current = session.history_request("c");
    session
        .insert_history(
            current,
            Page {
                items: (151..=200).map(|s| history_message(s, 0)).collect(),
                next_cursor: Some("151:c".into()),
            },
        )
        .await
        .unwrap();
    assert!(
        session
            .snapshot()
            .messages
            .iter()
            .any(|m| m.sequence == 175)
    );
    assert_eq!(
        session.snapshot().message_cursors["c"].as_deref(),
        Some("151:c")
    );
    let duplicate = session
        .insert_history(
            session.history_request("c"),
            Page {
                items: vec![history_message(175, 0)],
                next_cursor: None,
            },
        )
        .await
        .unwrap();
    assert!(!duplicate.added_rows);
    assert_eq!(duplicate.next_cursor, None);
    session.close().await;
}

#[tokio::test]
async fn history_requests_cannot_cross_host_sessions() {
    let first = fixture(snapshot(0, "first")).await;
    first.release.add_permits(1);
    let original = first.client.synced_session().await.unwrap();
    let request = original.history_request("c");
    original.close().await;
    let mut changed = snapshot(0, "second");
    changed["host_id"] = json!("another-host");
    let second = fixture(changed).await;
    second.release.add_permits(1);
    let current = second.client.synced_session().await.unwrap();
    assert!(matches!(
        current
            .insert_history(
                request,
                Page {
                    items: vec![history_message(1, 0)],
                    next_cursor: None,
                }
            )
            .await,
        Err(Error::ResyncRequired)
    ));
    assert_eq!(current.snapshot().messages.len(), 1);
    assert_eq!(current.snapshot().messages[0].text, "second");
    current.close().await;
}
