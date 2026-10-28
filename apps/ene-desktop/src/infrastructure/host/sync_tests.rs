use super::super::{connect, connect_fixture, transport::RuntimeConnection};
use super::*;
use axum::{Json, Router, routing::post};
use buffa::Message as _;
use ene_rpc as w;
use tokio::sync::{Semaphore, mpsc};

fn snapshot(seq: u64, text: &str) -> Snapshot {
    Snapshot {
        host_id: "h".into(),
        environment_id: "env".into(),
        cursor: Cursor {
            epoch: "e".into(),
            seq,
            ..Default::default()
        }
        .into(),
        settings: Settings {
            revision: 1,
            ..Default::default()
        }
        .into(),
        companions: vec![Companion {
            id: "c".into(),
            name: "ene".into(),
            revision: 1,
            availability: JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }],
        messages: vec![Message {
            id: "m".into(),
            companion_id: "c".into(),
            role: "companion".into(),
            status: "streaming".into(),
            text: text.into(),
            sequence: 2,
            created_at: "now".into(),
            in_reply_to: Some("u".into()),
            ..Default::default()
        }],
        message_cursors: [("c".into(), HistoryCursor::default())]
            .into_iter()
            .collect(),
        health: HostHealth {
            ready: true,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}
struct Fixture {
    client: Http,
    events: mpsc::Sender<Notification>,
    snapshots: Arc<Semaphore>,
    release: Arc<Semaphore>,
    closed: Arc<Semaphore>,
    current: Arc<std::sync::Mutex<Snapshot>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(current: Snapshot) -> Fixture {
    let (events, receiver) = mpsc::channel::<Notification>(512);
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
                impl Drop for Closed { fn drop(&mut self) { self.0.add_permits(1); } }
                let _closed = Closed(closed);
                started.add_permits(1);
                release.acquire().await.unwrap().forget();
                let snapshot = current.lock().unwrap().clone();
                let initial = w::HostEvent {
                    event: Some(w::host_event::Event::Initial(Box::new(w::InitialState {
                        subscription_id: "s".into(), snapshot: snapshot.into(), ..Default::default()
                    }))), ..Default::default()
                };
                yield Ok::<_, std::convert::Infallible>(frame(initial));
                let mut receiver = receiver;
                while let Some(notification) = receiver.recv().await {
                    let event = w::HostEvent {
                        event: Some(w::host_event::Event::Notification(Box::new(notification))),
                        ..Default::default()
                    };
                    yield Ok(frame(event));
                }
                yield Ok(vec![2,0,0,0,2,b'{',b'}']);
            };
            axum::response::Response::builder().header("content-type", "application/connect+proto")
                .body(axum::body::Body::from_stream(stream)).unwrap()
        }
    })).merge(connect_fixture::router(Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move |axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| {
            assert_eq!(query.get("connection_id").map(String::as_str), Some("s"));
            let current = snap_current.clone();
            let started = snap_started.clone();
            let release = snap_release.clone();
            async move {
                started.add_permits(1);
                release.acquire().await.unwrap().forget();
                Json(current.lock().unwrap().clone())
            }
        }))
        .route("/rpc/ene.host.v1.HostService/GetHistory", post(|axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            assert_eq!(query.get("cursor").map(String::as_str), Some("52:c"));
            Json(History { items: (1..52).map(|seq| history_message(seq,0)).collect(), ..Default::default() })
        }))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = connect(&RuntimeConnection {
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
fn delta(seq: u64, offset: u64, text: &str) -> Notification {
    Notification {
        cursor: Cursor {
            epoch: "e".into(),
            seq,
            ..Default::default()
        }
        .into(),
        companion_id: Some("c".into()),
        event: Some(w::notification::Event::MessageDelta(Box::new(
            MessageDelta {
                message_id: "m".into(),
                offset,
                delta: text.into(),
                ..Default::default()
            },
        ))),
        ..Default::default()
    }
}
fn settings(seq: u64) -> Notification {
    Notification {
        cursor: Cursor {
            epoch: "e".into(),
            seq,
            ..Default::default()
        }
        .into(),
        event: Some(w::notification::Event::SettingsUpdated(Box::new(
            Settings {
                revision: 1,
                ..Default::default()
            },
        ))),
        ..Default::default()
    }
}
#[tokio::test]
async fn subscription_buffers_snapshot_overlap_and_cancelled_waiters_cannot_interrupt_refresh() {
    let fixture = fixture(snapshot(1, "日")).await;
    let client = fixture.client.clone();
    let starting = tokio::spawn(async move { SyncedSession::connect(client.clone()).await });
    entered(&fixture.snapshots).await;
    fixture.events.send(delta(1, 999, "covered")).await.unwrap();
    fixture.events.send(delta(2, 1, "🙂")).await.unwrap();
    fixture.release.add_permits(1);
    let mut session = starting.await.unwrap().unwrap();
    assert_eq!(session.next().await.unwrap().cursor.seq, 2);
    assert_eq!(session.snapshot().messages[0].text, "日🙂");
    let applied = session.snapshot();
    assert_eq!(applied.cursor.seq, 2);
    assert_eq!(applied.messages[0].text, "日🙂");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), session.next())
            .await
            .is_err()
    );
    fixture.events.send(delta(3, 2, "。")).await.unwrap();
    assert_eq!(session.next().await.unwrap().cursor.seq, 3);
    let mut fresh = snapshot(4, "日🙂。");
    fresh.settings.get_or_insert_default().revision = 2;
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
    assert_eq!(session.next().await.unwrap().cursor.seq, 4);
    assert_eq!(session.snapshot().settings.revision, 2);
    assert_eq!(session.next().await.unwrap().cursor.seq, 5);
    assert_eq!(session.snapshot().messages[0].text, "日🙂。e\u{301}");
    assert_eq!(session.snapshot().cursor.seq, 5);
    let mut changed = snapshot(6, "日🙂。e\u{301}");
    changed.host_id = "replacement".into();
    *fixture.current.lock().unwrap() = changed;
    fixture.events.send(settings(6)).await.unwrap();
    entered(&fixture.snapshots).await;
    fixture.release.add_permits(1);
    assert!(
        matches!(session.next().await, Err(error) if error.code == connectrpc::ErrorCode::FailedPrecondition)
    );
    session.close().await;
}
#[tokio::test]
async fn notification_overflow_during_refresh_requires_a_new_subscription() {
    let fixture = fixture(snapshot(0, "")).await;
    fixture.release.add_permits(1);
    let mut session = SyncedSession::connect(fixture.client.clone())
        .await
        .unwrap();
    entered(&fixture.snapshots).await;
    fixture.events.send(settings(1)).await.unwrap();
    entered(&fixture.snapshots).await;
    for seq in 2..=301 {
        fixture.events.send(delta(seq, seq - 2, "x")).await.unwrap();
    }
    entered(&fixture.closed).await;
    fixture.release.add_permits(1);
    assert!(
        matches!(session.next().await, Err(error) if error.code == connectrpc::ErrorCode::FailedPrecondition)
    );
    session.close().await;
}
#[tokio::test]
async fn broken_notifications_reject_the_session_without_applying_the_delta() {
    let mut wrong_epoch = delta(1, 1, "bad");
    wrong_epoch.cursor.get_or_insert_default().epoch = "replacement".into();
    let mut wrong_companion = delta(1, 1, "bad");
    wrong_companion.companion_id = Some("other".into());
    let mut missing_cursor = delta(1, 1, "bad");
    missing_cursor.cursor = Default::default();
    for event in [
        delta(2, 1, "gap"),
        delta(1, 3, "byte offset"),
        wrong_epoch,
        wrong_companion,
        missing_cursor,
    ] {
        let fixture = fixture(snapshot(0, "日")).await;
        fixture.release.add_permits(1);
        let mut session = SyncedSession::connect(fixture.client.clone())
            .await
            .unwrap();
        let before = session.snapshot();
        fixture.events.send(event).await.unwrap();
        assert!(
            matches!(session.next().await, Err(error) if error.code == connectrpc::ErrorCode::FailedPrecondition)
        );
        assert_eq!(session.snapshot(), before);
        session.close().await;
    }
}
fn history_message(sequence: u64, bytes: usize) -> Message {
    Message {
        id: format!("history-{sequence}"),
        companion_id: "c".into(),
        role: "user".into(),
        status: "completed".into(),
        text: "x".repeat(bytes),
        sequence,
        created_at: "now".into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn review_host_storage_failure_is_applied_before_delivering_notice() {
    let fixture = fixture(snapshot(0, "before")).await;
    fixture.release.add_permits(1);
    let mut session = SyncedSession::connect(fixture.client.clone())
        .await
        .unwrap();
    fixture
        .events
        .send(Notification {
            cursor: Cursor {
                epoch: "e".into(),
                seq: 1,
                ..Default::default()
            }
            .into(),
            event: Some(w::notification::Event::HostStorageFailed(Box::new(
                PersistenceFailure {
                    code: "storage_error".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(matches!(
        session.next().await.unwrap().event,
        Some(w::notification::Event::HostStorageFailed(_))
    ));
    let state = session.snapshot();
    assert_eq!(state.cursor.seq, 1);
    assert!(!state.health.ready);
    assert!(state.health.recovery_required);
    let availability: serde_json::Value =
        serde_json::from_str(&state.companions[0].availability.json).unwrap();
    assert_eq!(availability["chat"]["ready"], false);
    assert_eq!(availability["chat"]["reasons"][0]["code"], "storage_error");
    session.close().await;
}
#[tokio::test]
async fn review_refresh_keeps_gaps_reachable_after_exhausted_history() {
    let mut initial = snapshot(0, "");
    initial.messages = vec![history_message(1, 0)];
    let fixture = fixture(initial).await;
    fixture.release.add_permits(1);
    let session = SyncedSession::connect(fixture.client.clone())
        .await
        .unwrap();
    session
        .insert_history(
            session.history_request("c"),
            History {
                items: vec![history_message(1, 0)],
                next_cursor: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut fresh = snapshot(1, "");
    fresh.messages = (52..=101)
        .map(|seq| history_message(seq, 0))
        .collect::<Vec<_>>();
    fresh.message_cursors.get_mut("c").unwrap().value = Some("52:c".into());
    *fixture.current.lock().unwrap() = fresh;
    fixture.release.add_permits(1);
    session.refresh().await.unwrap();
    let merged = session.snapshot();
    assert_eq!(merged.messages.len(), 51);
    assert_eq!(merged.message_cursors["c"].value.as_deref(), Some("52:c"));
    let page = fixture
        .client
        .get_history_with_options(
            ene_rpc::HistoryRequest {
                companion_id: "c".into(),
                cursor: (merged.message_cursors["c"].value.as_deref()).map(Into::into),
                limit: None,
                ..Default::default()
            },
            call_options(5),
        )
        .await
        .unwrap()
        .into_owned();
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
    assert!(complete.message_cursors["c"].value.is_none());
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
    initial.messages = (101..=150)
        .map(|s| history_message(s, 0))
        .collect::<Vec<_>>();
    initial.message_cursors.get_mut("c").unwrap().value = Some("101:c".into());
    let fixture = fixture(initial).await;
    fixture.release.add_permits(1);
    let session = SyncedSession::connect(fixture.client.clone())
        .await
        .unwrap();
    let old = session.history_request("c");
    let mut fresh = snapshot(1, "");
    fresh.messages = (201..=250)
        .map(|s| history_message(s, 0))
        .collect::<Vec<_>>();
    fresh.message_cursors.get_mut("c").unwrap().value = Some("201:c".into());
    *fixture.current.lock().unwrap() = fresh;
    fixture.release.add_permits(1);
    session.refresh().await.unwrap();
    let applied = session
        .insert_history(
            old,
            History {
                items: (51..=100).map(|s| history_message(s, 0)).collect(),
                next_cursor: Some("51:c".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(applied.added_rows);
    assert_eq!(applied.next_cursor.as_deref(), Some("201:c"));
    assert_eq!(
        session.snapshot().message_cursors["c"].value.as_deref(),
        Some("201:c")
    );
    assert!(session.snapshot().messages.iter().any(|m| m.sequence == 51));
    let current = session.history_request("c");
    session
        .insert_history(
            current,
            History {
                items: (151..=200).map(|s| history_message(s, 0)).collect(),
                next_cursor: Some("151:c".into()),
                ..Default::default()
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
        session.snapshot().message_cursors["c"].value.as_deref(),
        Some("151:c")
    );
    let duplicate = session
        .insert_history(
            session.history_request("c"),
            History {
                items: vec![history_message(175, 0)],
                next_cursor: None,
                ..Default::default()
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
    let original = SyncedSession::connect(first.client.clone()).await.unwrap();
    let request = original.history_request("c");
    original.close().await;
    let mut changed = snapshot(0, "second");
    changed.host_id = "another-host".into();
    let second = fixture(changed).await;
    second.release.add_permits(1);
    let current = SyncedSession::connect(second.client.clone()).await.unwrap();
    assert!(matches!(
        current
            .insert_history(
                request,
                History {
                    items: vec![history_message(1, 0)],
                    next_cursor: None,
                    ..Default::default()
                }
            )
            .await,
        Err(error) if error.code == connectrpc::ErrorCode::FailedPrecondition
    ));
    assert_eq!(current.snapshot().messages.len(), 1);
    assert_eq!(current.snapshot().messages[0].text, "second");
    current.close().await;
}
