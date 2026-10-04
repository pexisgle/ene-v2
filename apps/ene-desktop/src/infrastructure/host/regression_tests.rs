use super::mutations::MutationQueue;
use super::*;
use axum::{
    Json, Router,
    routing::{get, patch, put},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn snapshot() -> Snapshot {
    serde_json::from_value(json!({
        "host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"e","seq":0},
        "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,"model_assignments":{},"availability":{},"revision":1}],
        "messages":[],"message_cursors":{},"tasks":[],"questions":[],"grants":[],
        "settings":{"revision":1,"connections":[{"id":"p","kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{},"credential_state":"stored","capabilities":null,"last_check":null}]}
    })).unwrap()
}
async fn serve(router: Router) -> (Http, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Http::new(&ene_api_types::host::RuntimeConnection {
        endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
        token: "fixture".into(),
    })
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (http, server)
}

#[tokio::test]
async fn credential_revoke_completes_while_a_model_check_is_still_running() {
    let snapshot = snapshot();
    let saved = snapshot.clone();
    let stored = Arc::new(AtomicBool::new(true));
    let deleted = stored.clone();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let arrived = entered.clone();
    let router = Router::new()
        .route(
            "/v1/snapshot",
            get(move || {
                let saved = saved.clone();
                async move { Json(saved) }
            }),
        )
        .route(
            "/v1/connections/p/check",
            axum::routing::post(move || {
                let arrived = arrived.clone();
                async move {
                    arrived.add_permits(1);
                    std::future::pending::<()>().await;
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/v1/credentials/p",
            axum::routing::delete(move || {
                let deleted = deleted.clone();
                async move {
                    deleted.store(false, Ordering::SeqCst);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let (http, server) = serve(router).await;
    let queue = MutationQueue::new();
    let mut jobs = tokio::task::JoinSet::new();
    session::spawn_action(
        &mut jobs,
        &queue,
        http.clone(),
        snapshot.clone(),
        "c".into(),
        Command::Check("p".into()),
        None,
    );
    tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    session::spawn_action(
        &mut jobs,
        &queue,
        http,
        snapshot,
        "c".into(),
        Command::RevokeCredential("p".into()),
        None,
    );
    let completed = tokio::time::timeout(Duration::from_secs(2), jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        completed,
        session::Job::Action(_, Ok(ActionResult::Done(..)))
    ));
    assert!(!stored.load(Ordering::SeqCst));
    assert_eq!(jobs.len(), 1, "model check is still running");
    jobs.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn credential_revoke_follows_admitted_save_even_when_its_waiter_is_dropped() {
    let snapshot = snapshot();
    let stored = Arc::new(AtomicBool::new(true));
    let changed = stored.clone();
    let deleted = stored.clone();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let arrived = entered.clone();
    let gate = release.clone();
    let reply = snapshot.settings.clone();
    let companion = snapshot.companions[0].clone();
    let router = Router::new()
        .route("/v1/snapshot", get(move || {let snapshot=snapshot.clone(); async move {Json(snapshot)}}))
        .route("/v1/providers", get(|| async { Json(json!([{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com"],"state":"ready","error":null,"revision":"1","capabilities":{"streaming_text":true,"model_list":true}}])) }))
        .route("/v1/settings", patch(move || { let reply=reply.clone(); async move { Json(reply) } }))
        .route("/v1/companions/c", patch(move || { let arrived=arrived.clone(); let gate=gate.clone(); let companion=companion.clone(); async move {
            arrived.add_permits(1);
            gate.acquire().await.unwrap().forget();
            Json(companion)
        }}))
        .route("/v1/credentials/p", put(move || { let changed=changed.clone(); async move { changed.store(true, Ordering::SeqCst); Json(json!({"credential_state":"stored"})) } })
            .delete(move || { let deleted=deleted.clone(); async move { deleted.store(false, Ordering::SeqCst); axum::http::StatusCode::NO_CONTENT } }));
    let (http, server) = serve(router).await;
    let queue = MutationQueue::new();
    let saving = queue.submit(
        http.clone(),
        "c".into(),
        Command::Configure {
            connection_id: Some("p".into()),
            creation_id: None,
            provider: "openai".into(),
            endpoint: "https://api.openai.com/v1".into(),
            model: "m".into(),
            options: json!({}),
            update_connection: true,
            credential: Some(CredentialEdit::Secret("pending-key".into())),
        },
    );
    tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    // Subscription failure drops its JoinSet waiter, but the admitted write
    // must finish before a revoke submitted by the next session.
    drop(saving);
    let mut revoking = queue.submit(http, "c".into(), Command::RevokeCredential("p".into()));
    let premature = tokio::time::timeout(Duration::from_millis(100), &mut revoking)
        .await
        .is_ok();
    release.add_permits(1);
    assert!(
        !premature,
        "revoke must not report success while the earlier save can restore the key"
    );
    tokio::time::timeout(Duration::from_secs(2), revoking)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!stored.load(Ordering::SeqCst));
    server.abort();
}

#[tokio::test]
async fn permission_revoke_observes_grant_created_by_an_earlier_pending_save() {
    let snapshot = snapshot();
    let stored = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let arrived = entered.clone();
    let gate = release.clone();
    let changed = stored.clone();
    let queried = stored.clone();
    let deleted = stored.clone();
    let grant = json!({"id":"g","companion_id":"c","kind":"data_transfer","connection_id":"p","destinations":["https://api.openai.com"],"data_kinds":["conversation"],"purpose":"conversation","status":"active","revision":1,"invalidation_reason":null,"connection_fingerprint":{}});
    let created = grant.clone();
    let router=Router::new()
        .route("/v1/snapshot", get(move || {let snapshot=snapshot.clone(); async move {Json(snapshot)}}))
        .route("/v1/providers",get(move || {let arrived=arrived.clone();let gate=gate.clone();async move {
            arrived.add_permits(1);gate.acquire().await.unwrap().forget();
            Json(json!([{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com"],"state":"ready","error":null,"revision":"1","capabilities":{"streaming_text":true,"model_list":true}}]))
        }}))
        .route("/v1/grants",get(move || {let queried=queried.clone();let grant=grant.clone();async move {
            Json(if queried.load(Ordering::SeqCst) {json!([grant])} else {json!([])})
        }}).post(move || {let changed=changed.clone();let created=created.clone();async move {
            changed.store(true,Ordering::SeqCst);
            Json(json!({"grant":created,"questions":[]}))
        }}))
        .route("/v1/grants/g", axum::routing::delete(move || {let deleted=deleted.clone();async move {
            deleted.store(false,Ordering::SeqCst); axum::http::StatusCode::NO_CONTENT
        }}));
    let (http, server) = serve(router).await;
    let queue = MutationQueue::new();
    let saving = queue.submit(
        http.clone(),
        "c".into(),
        Command::Permission {
            connection: "p".into(),
            conversation: true,
            check: false,
        },
    );
    tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let mut revoking = queue.submit(http, "c".into(), Command::RevokePermissions("p".into()));
    let premature = tokio::time::timeout(Duration::from_millis(100), &mut revoking)
        .await
        .is_ok();
    release.add_permits(1);
    assert!(!premature);
    tokio::time::timeout(Duration::from_secs(2), saving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), revoking)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!stored.load(Ordering::SeqCst));
    server.abort();
}

#[tokio::test]
async fn snapshot_refresh_preserves_loaded_and_exhausted_history_cursors_per_companion() {
    for cursor in [Some("51:c".to_string()), None] {
        let mut snapshot = snapshot();
        let message = |n| {
            serde_json::from_value::<Message>(json!({
                "id":format!("m{n}"),"companion_id":"c","role":"user","text":format!("text{n}"),
                "status":"completed","created_at":"2026-10-04T00:00:00Z","sequence":n,
                "resources":[],"in_reply_to":null,"task_id":null,"question_id":null,"failure":null
            }))
            .unwrap()
        };
        snapshot.messages = (101..=150).map(message).collect();
        snapshot
            .message_cursors
            .insert("c".into(), Some("101:c".into()));
        let mut other = snapshot.companions[0].clone();
        other.id = "other".into();
        snapshot.companions.push(other);
        snapshot
            .message_cursors
            .insert("other".into(), Some("101:other".into()));
        let mut previous = Replica::from_snapshot(&snapshot);
        for n in 51..=100 {
            let row = message(n);
            previous.messages.insert(row.id.clone(), row);
        }
        let observed = Arc::new(std::sync::Mutex::new(None));
        let requested = observed.clone();
        let router = Router::new()
            .route(
                "/v1/snapshot",
                get(move || {
                    let snapshot = snapshot.clone();
                    async move { Json(snapshot) }
                }),
            )
            .route(
                "/v1/companions/c/messages",
                get(
                    move |axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| {
                        let requested = requested.clone();
                        async move {
                            *requested.lock().unwrap() = Some(query["cursor"].clone());
                            Json(json!({"items":[],"next_cursor":null}))
                        }
                    },
                ),
            );
        let (http, server) = serve(router).await;
        let loaded = std::collections::BTreeMap::from([("c".into(), cursor.clone())]);
        let current = session::refresh_snapshot(&http, "subscription", &loaded)
            .await
            .unwrap();
        assert_eq!(
            current.message_cursors["other"].as_deref(),
            Some("101:other")
        );
        let restored = session::refreshed(&http, &current, previous).await.unwrap();
        assert_eq!(restored.messages.len(), 100);
        settings::action(http, current, "c".into(), Command::History)
            .await
            .unwrap();
        assert_eq!(*observed.lock().unwrap(), cursor);
        server.abort();
    }
}

#[tokio::test]
async fn new_draft_does_not_overwrite_a_partially_saved_connection() {
    let (commands, mut receiver) = channel::unbounded_channel();
    let (_sender, events) = mpsc::channel();
    let controller = Controller {
        commands,
        events,
        latest_requests: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        latest_view: Arc::new(std::sync::Mutex::new(None)),
        creation_ids: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        stop: CancellationToken::new(),
        thread: None,
    };
    let configure = |provider: &str| Command::Configure {
        connection_id: None,
        creation_id: None,
        provider: provider.into(),
        endpoint: format!("https://{provider}.test/v1"),
        model: "model".into(),
        options: json!({}),
        update_connection: true,
        credential: None,
    };
    let identity = |request: &Request| match &request.command {
        Command::Configure { creation_id, .. } => creation_id.clone().unwrap(),
        _ => unreachable!(),
    };
    controller.request_at("c", configure("first"), 1).unwrap();
    let first = receiver.try_recv().unwrap();
    controller.request_at("c", configure("first"), 1).unwrap();
    assert_eq!(
        identity(&first),
        identity(&receiver.try_recv().unwrap()),
        "retry must retain its UUID"
    );
    // A partial or uncertain save has already created the first connection.
    // Choosing a different provider starts a new draft with no selected ID.
    controller.request_at("c", configure("second"), 2).unwrap();
    let second = receiver.try_recv().unwrap();
    assert_ne!(identity(&first), identity(&second));
    let mut fresh = snapshot();
    fresh.settings.connections[0].id = identity(&first);
    fresh.settings.connections[0].adapter = "first".into();
    fresh.settings.connections[0].endpoint = "https://first.test/v1".into();
    let expected_id = identity(&second);
    let original = fresh.settings.connections[0].clone();
    let companion = fresh.companions[0].clone();
    let router = Router::new()
        .route("/v1/providers", get(|| async { Json(json!([{
            "id":"second","name":"Second","kind":"plugin","endpoint":"https://second.test/v1","options":{},"settings":[],
            "requires_credentials":false,"environment_variable":null,"destinations":["https://second.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}
        }])) }))
        .route("/v1/settings", patch(move |Json(input): Json<ene_api_types::host::SettingsPatch>| {
            let original = original.clone(); let expected_id = expected_id.clone();
            async move {
                assert_eq!(input.connections.len(), 2);
                assert_eq!(input.connections[0].id.as_deref(), Some(original.id.as_str()));
                assert_eq!(input.connections[0].adapter, original.adapter);
                assert_eq!(input.connections[0].endpoint, original.endpoint);
                assert!(!input.connections[0].create);
                let created = &input.connections[1];
                assert_eq!(created.id.as_deref(), Some(expected_id.as_str()));
                assert!(created.create);
                Json(ene_api_types::Settings { revision: 2, connections: vec![original,
                    ene_api_types::Connection { id: expected_id, kind: created.kind.clone(),
                        adapter: created.adapter.clone(), endpoint: created.endpoint.clone(),
                        options: created.options.clone(), credential_state: "missing".into(),
                        capabilities: None, last_check: None,
                    }] })
            }
        }))
        .route("/v1/companions/c", patch(move || { let companion = companion.clone(); async move { Json(companion) } }));
    let (http, server) = serve(router).await;
    let second_id = identity(&second);
    assert!(
        action(http, fresh, "c".into(), second.command)
            .await
            .is_ok()
    );
    server.abort();
    let _ = server.await;
    controller.discard_connection_draft("c");
    controller.request_at("c", configure("second"), 3).unwrap();
    let replacement = receiver.try_recv().unwrap();
    assert_ne!(
        second_id,
        identity(&replacement),
        "resetting the same provider starts a new draft"
    );
}
