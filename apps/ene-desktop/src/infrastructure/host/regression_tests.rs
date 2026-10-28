use super::mutations::MutationQueue;
use super::*;
use axum::{Json, Router, routing::post};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn snapshot() -> Snapshot {
    serde_json::from_value(json!({
        "host_id":"h","environment_id":"e","capabilities":{"json":"{}"},"cursor":{"epoch":"e","seq":0},
        "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,"model_assignments":{},"availability":{"json":"{}"},"revision":1}],
        "messages":[],"message_cursors":{},"grants":[],
        "settings":{"revision":1,"connections":[{"id":"p","kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{"json":"{}"},"credential_state":"stored","capabilities":null,"last_check":null}]}
    })).unwrap()
}
async fn serve(router: Router) -> (Http, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = connect(&crate::infrastructure::host::transport::RuntimeConnection {
        endpoint: format!("http://{}/rpc", listener.local_addr().unwrap()),
        token: "fixture".into(),
    })
    .unwrap();
    let router = router.route("/rpc/ene.host.v1.HostService/GetAvailability", post(|| async { Json(json!({"companion_id":"c","connection_id":"p","settings_revision":1,"provider_revision":"1","destinations":["https://api.openai.com"],"connection_fingerprint":{"json":"{}"},"operations":[{"operation":"conversation","purpose":"conversation","data_kinds":["conversation"],"authorization":"missing","dependency":"ready","busy":false,"runnable":false},{"operation":"connection_check","purpose":"connection_check","data_kinds":["test_input"],"authorization":"missing","dependency":"ready","busy":false,"runnable":false},{"operation":"model_list","purpose":"connection_check","data_kinds":["metadata"],"authorization":"missing","dependency":"ready","busy":false,"runnable":false}]})) }));
    let server = tokio::spawn(async move {
        axum::serve(listener, super::connect_fixture::router(router))
            .await
            .unwrap()
    });
    (http, server)
}

#[tokio::test]
async fn companion_draft_keeps_its_revision_when_the_queue_refreshes() {
    let mut current = snapshot();
    current.companions[0].revision = 2;
    current.companions[0].mind = "remote mind".into();
    let saved = Arc::new(std::sync::Mutex::new(current.companions[0].clone()));
    let changed = saved.clone();
    let router = Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move || { let current = current.clone(); async move { Json(current) } }))
        .route("/rpc/ene.host.v1.HostService/UpdateCompanion", post(move |Json(input): Json<ene_rpc::UpdateCompanionRequest>| {
            let changed = changed.clone();
            async move {
                let mut saved = changed.lock().unwrap();
                if input.revision != saved.revision {
                    return (axum::http::StatusCode::CONFLICT, Json(json!({"error":{"code":"revision_conflict","message":"changed","details":{}}})));
                }
                saved.name = input.name.unwrap(); saved.mind = input.mind.unwrap(); saved.revision += 1;
                (axum::http::StatusCode::OK, Json(serde_json::to_value(&*saved).unwrap()))
            }
        }));
    let (http, server) = serve(router).await;
    let result = MutationQueue::new()
        .submit(
            http,
            "c".into(),
            Command::Companion {
                revision: 1,
                name: "draft name".into(),
                mind: "old mind".into(),
                body: None,
                voice: None,
            },
            Uuid::new_v4().to_string(),
            "h".into(),
        )
        .await
        .unwrap();
    assert!(result.is_err());
    let saved = saved.lock().unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.name, "ene");
    assert_eq!(saved.mind, "remote mind");
    server.abort();
}

#[tokio::test]
async fn connection_draft_keeps_its_revision_when_the_queue_refreshes() {
    let mut current = snapshot();
    current.settings.get_or_insert_default().revision = 2;
    current.settings.get_or_insert_default().connections[0].options = ene_rpc::JsonDocument {
        json: json!({"remote":true}).to_string(),
        ..Default::default()
    }
    .into();
    let saved = Arc::new(std::sync::Mutex::new(
        current.settings.as_option().unwrap().clone(),
    ));
    let changed = saved.clone();
    let router = Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move || { let current = current.clone(); async move { Json(current) } }))
        .route("/rpc/ene.host.v1.HostService/ListProviders", post(|| async { Json(json!({"items":[{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{"json":"{}"},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com"],"state":"ready","error":null,"revision":"1","capabilities":{"streaming_text":true,"model_list":true}}]})) }))
        .route("/rpc/ene.host.v1.HostService/UpdateSettings", post(move |Json(input): Json<ene_rpc::UpdateSettingsRequest>| {
            let changed = changed.clone();
            async move {
                let mut saved = changed.lock().unwrap();
                if input.revision != saved.revision {
                    return (axum::http::StatusCode::CONFLICT, Json(json!({"error":{"code":"revision_conflict","message":"changed","details":{}}})));
                }
                saved.connections[0].options = input.connections.items[0].options.clone(); saved.revision += 1;
                (axum::http::StatusCode::OK, Json(serde_json::to_value(&*saved).unwrap()))
            }
        }));
    let (http, server) = serve(router).await;
    let result = MutationQueue::new()
        .submit(
            http,
            "c".into(),
            Command::Configure {
                settings_revision: 1,
                companion_revision: 1,
                connection_id: Some("p".into()),
                creation_id: None,
                provider: "openai".into(),
                endpoint: "https://api.openai.com/v1".into(),
                model: String::new(),
                options: ene_rpc::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                },
                update_connection: true,
                credential: None,
            },
            Uuid::new_v4().to_string(),
            "h".into(),
        )
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        saved.lock().unwrap().connections[0].options.json,
        json!({"remote":true}).to_string()
    );
    assert_eq!(saved.lock().unwrap().revision, 2);
    server.abort();
}

#[tokio::test]
async fn permission_save_sends_only_the_scope_presented_to_the_user() {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let observed = captured.clone();
    let current = snapshot();
    let router = Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move || { let current = current.clone(); async move { Json(current) } }))
        .route("/rpc/ene.host.v1.HostService/ListProviders", post(|| async { Json(json!({"items":[{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{"json":"{}"},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com","https://undisplayed.test"],"state":"ready","error":null,"revision":"new","capabilities":{"streaming_text":true,"model_list":true}}]})) }))
        .route("/rpc/ene.host.v1.HostService/CreateGrant", axum::routing::post(move |Json(input): Json<ene_rpc::CreateGrantRequest>| {
            let observed = observed.clone(); async move {
                *observed.lock().unwrap() = Some(input);
                (axum::http::StatusCode::CONFLICT, Json(json!({"error":{"code":"revision_conflict","message":"changed","details":{}}})))
            }
        }));
    let (http, server) = serve(router).await;
    let result = MutationQueue::new()
        .submit(
            http,
            "c".into(),
            Command::Permission {
                consent: ene_rpc::Consent {
                    settings_revision: 1,
                    provider_revision: "old".into(),
                    ..Default::default()
                },
                destinations: vec!["https://api.openai.com".into()],
                connection: "p".into(),
                conversation: true,
                check: false,
            },
            Uuid::new_v4().to_string(),
            "h".into(),
        )
        .await
        .unwrap();
    assert!(result.is_err());
    let captured = captured.lock().unwrap();
    let grant = captured.as_ref().unwrap();
    assert_eq!(grant.destinations, vec!["https://api.openai.com"]);
    assert_eq!(grant.consent.provider_revision, "old");
    assert_eq!(grant.consent.settings_revision, 1);
    server.abort();
}

#[tokio::test]
async fn model_assignment_conflicts_preserve_remote_changes_and_applied_retries_skip_rewrites() {
    for already_applied in [false, true] {
        let mut current = snapshot();
        current.settings.get_or_insert_default().revision = 2;
        current.companions[0].revision = 2;
        current.companions[0]
            .model_assignments
            .get_or_insert_default()
            .companion = ene_rpc::ModelAssignment {
            connection_id: "p".into(),
            model_id: if already_applied { "draft" } else { "remote" }.into(),
            ..Default::default()
        }
        .into();
        let saved = Arc::new(std::sync::Mutex::new(current.companions[0].clone()));
        let changed = saved.clone();
        let settings = current.settings.as_option().unwrap().clone();
        let router = Router::new()
            .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move || { let current = current.clone(); async move { Json(current) } }))
            .route("/rpc/ene.host.v1.HostService/ListProviders", post(|| async { Json(json!({"items":[{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{"json":"{}"},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com"],"state":"ready","error":null,"revision":"1","capabilities":{"streaming_text":true,"model_list":true}}]})) }))
            .route("/rpc/ene.host.v1.HostService/UpdateSettings", post(move |Json(input): Json<ene_rpc::UpdateSettingsRequest>| {
                let settings = settings.clone(); async move {
                    // Simulate a replay of the first, already-applied step.
                    assert_eq!(input.revision, 1); Json(settings)
                }
            }))
            .route("/rpc/ene.host.v1.HostService/UpdateCompanion", post(move |Json(input): Json<ene_rpc::UpdateCompanionRequest>| {
                let changed = changed.clone(); async move {
                    assert!(!already_applied, "already-applied assignment must not be rewritten");
                    let mut saved = changed.lock().unwrap();
                    if input.revision != saved.revision {
                        return (axum::http::StatusCode::CONFLICT, Json(json!({"error":{"code":"revision_conflict","message":"changed","details":{}}})));
                    }
                    let Some(ene_rpc::assignment_patch::Change::Set(assignment)) = input.companion_assignment.into_option().unwrap().change else { panic!("set assignment"); };
                    saved.model_assignments.get_or_insert_default().companion = (*assignment).into();
                    saved.revision += 1;
                    (axum::http::StatusCode::OK, Json(serde_json::to_value(&*saved).unwrap()))
                }
            }));
        let (http, server) = serve(router).await;
        let result = MutationQueue::new()
            .submit(
                http,
                "c".into(),
                Command::Configure {
                    settings_revision: 1,
                    companion_revision: 1,
                    connection_id: Some("p".into()),
                    creation_id: None,
                    provider: "openai".into(),
                    endpoint: "https://api.openai.com/v1".into(),
                    model: "draft".into(),
                    options: ene_rpc::JsonDocument {
                        json: "{}".into(),
                        ..Default::default()
                    },
                    update_connection: true,
                    credential: None,
                },
                Uuid::new_v4().to_string(),
                "h".into(),
            )
            .await
            .unwrap();
        assert_eq!(result.is_ok(), already_applied);
        assert_eq!(saved.lock().unwrap().revision, 2);
        assert_eq!(
            saved
                .lock()
                .unwrap()
                .model_assignments
                .companion
                .as_option()
                .unwrap()
                .model_id,
            if already_applied { "draft" } else { "remote" }
        );
        server.abort();
    }
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
            "/rpc/ene.host.v1.HostService/GetSnapshot",
            post(move || {
                let saved = saved.clone();
                async move { Json(saved) }
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/CheckConnection",
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
            "/rpc/ene.host.v1.HostService/RevokeCredentials",
            axum::routing::post(move || {
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
        context(Command::Check("p".into())),
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
        context(Command::RevokeCredential("p".into())),
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
    let reply = snapshot.settings.as_option().unwrap().clone();
    let companion = snapshot.companions[0].clone();
    let router = Router::new()
        .route("/rpc/ene.host.v1.HostService/GetSnapshot", post(move || {let snapshot=snapshot.clone(); async move {Json(snapshot)}}))
        .route("/rpc/ene.host.v1.HostService/ListProviders", post(|| async { Json(json!({"items":[{"id":"openai","name":"OpenAI","kind":"builtin","endpoint":"https://api.openai.com/v1","options":{"json":"{}"},"settings":[],"requires_credentials":true,"environment_variable":null,"destinations":["https://api.openai.com"],"state":"ready","error":null,"revision":"1","capabilities":{"streaming_text":true,"model_list":true}}]})) }))
        .route("/rpc/ene.host.v1.HostService/UpdateSettings", post(move || { let reply=reply.clone(); async move { Json(reply) } }))
        .route("/rpc/ene.host.v1.HostService/UpdateCompanion", post(move || { let arrived=arrived.clone(); let gate=gate.clone(); let companion=companion.clone(); async move {
            arrived.add_permits(1);
            gate.acquire().await.unwrap().forget();
            Json(companion)
        }}))
        .route("/rpc/ene.host.v1.HostService/RegisterCredentials", post(move || { let changed=changed.clone(); async move { changed.store(true, Ordering::SeqCst); Json(json!({"credential_state":"stored"})) } })
            ).route("/rpc/ene.host.v1.HostService/RevokeCredentials", axum::routing::post(move || { let deleted=deleted.clone(); async move { deleted.store(false, Ordering::SeqCst); axum::http::StatusCode::NO_CONTENT } }));
    let (http, server) = serve(router).await;
    let queue = MutationQueue::new();
    let saving = queue.submit(
        http.clone(),
        "c".into(),
        Command::Configure {
            settings_revision: 1,
            companion_revision: 1,
            connection_id: Some("p".into()),
            creation_id: None,
            provider: "openai".into(),
            endpoint: "https://api.openai.com/v1".into(),
            model: "m".into(),
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            },
            update_connection: true,
            credential: Some(CredentialEdit::Secret("pending-key".into())),
        },
        Uuid::new_v4().to_string(),
        "h".into(),
    );
    tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    // Subscription failure drops its JoinSet waiter, but the admitted write
    // must finish before a revoke submitted by the next session.
    drop(saving);
    let mut revoking = queue.submit(
        http,
        "c".into(),
        Command::RevokeCredential("p".into()),
        Uuid::new_v4().to_string(),
        "h".into(),
    );
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
    let grant = json!({"id":"g","companion_id":"c","kind":"data_transfer","connection_id":"p","destinations":["https://api.openai.com"],"data_kinds":["conversation"],"purpose":"conversation","status":"active","revision":1,"invalidation_reason":null,"connection_fingerprint":{"json":"{}"}});
    let created = grant.clone();
    let router = Router::new()
        .route(
            "/rpc/ene.host.v1.HostService/GetSnapshot",
            post(move || {
                let snapshot = snapshot.clone();
                async move { Json(snapshot) }
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/ListGrants",
            post(move || {
                let queried = queried.clone();
                let grant = grant.clone();
                async move {
                    Json(if queried.load(Ordering::SeqCst) {
                        json!({"items":[grant]})
                    } else {
                        json!({"items":[]})
                    })
                }
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/CreateGrant",
            post(move || {
                let arrived = arrived.clone();
                let gate = gate.clone();
                let changed = changed.clone();
                let created = created.clone();
                async move {
                    arrived.add_permits(1);
                    gate.acquire().await.unwrap().forget();
                    changed.store(true, Ordering::SeqCst);
                    Json(json!({"grant":created}))
                }
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/RevokeGrant",
            axum::routing::post(move || {
                let deleted = deleted.clone();
                async move {
                    deleted.store(false, Ordering::SeqCst);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let (http, server) = serve(router).await;
    let queue = MutationQueue::new();
    let saving = queue.submit(
        http.clone(),
        "c".into(),
        Command::Permission {
            consent: ene_rpc::Consent {
                settings_revision: 1,
                provider_revision: "1".into(),
                ..Default::default()
            },
            destinations: vec!["https://api.openai.com".into()],
            connection: "p".into(),
            conversation: true,
            check: false,
        },
        Uuid::new_v4().to_string(),
        "h".into(),
    );
    tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let mut revoking = queue.submit(
        http,
        "c".into(),
        Command::RevokePermissions("p".into()),
        Uuid::new_v4().to_string(),
        "h".into(),
    );
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
async fn snapshot_refresh_restarts_pagination_without_hiding_missing_intervals() {
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
    snapshot.message_cursors.insert(
        "c".into(),
        ene_rpc::HistoryCursor {
            value: Some("101:c".into()),
            ..Default::default()
        },
    );
    let mut other = snapshot.companions[0].clone();
    other.id = "other".into();
    snapshot.companions.push(other);
    snapshot.message_cursors.insert(
        "other".into(),
        ene_rpc::HistoryCursor {
            value: Some("101:other".into()),
            ..Default::default()
        },
    );
    let initial_snapshot = snapshot.clone();
    snapshot.cursor.get_or_insert_default().seq = 1;
    let observed = Arc::new(std::sync::Mutex::new(None));
    let requested = observed.clone();
    let router = Router::new()
        .route(
            "/rpc/ene.host.v1.HostService/GetSnapshot",
            post(move || {
                let snapshot = snapshot.clone();
                async move { Json(snapshot) }
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/GetHistory",
            post(move |Json(query): Json<ene_rpc::HistoryRequest>| {
                let requested = requested.clone();
                async move {
                    *requested.lock().unwrap() = query.cursor.clone();
                    Json(json!({"items":[],"next_cursor":null}))
                }
            }),
        );
    use buffa::Message as _;
    use futures_util::StreamExt as _;
    let initial = ene_rpc::HostEvent {
        event: Some(ene_rpc::host_event::Event::Initial(Box::new(
            ene_rpc::InitialState {
                subscription_id: "subscription".into(),
                snapshot: initial_snapshot.into(),
                ..Default::default()
            },
        ))),
        ..Default::default()
    }
    .encode_to_vec();
    let mut frame = vec![0];
    frame.extend_from_slice(&u32::try_from(initial.len()).unwrap().to_be_bytes());
    frame.extend(initial);
    let router = Router::new()
        .route(
            "/rpc/ene.host.v1.HostService/Subscribe",
            post(move || {
                let frame = frame.clone();
                async move {
                    let stream = futures_util::stream::once(async {
                        Ok::<_, std::convert::Infallible>(frame)
                    })
                    .chain(futures_util::stream::pending());
                    axum::response::Response::builder()
                        .header("content-type", "application/connect+proto")
                        .body(axum::body::Body::from_stream(stream))
                        .unwrap()
                }
            }),
        )
        .merge(super::connect_fixture::router(router));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = connect(&crate::infrastructure::host::transport::RuntimeConnection {
        endpoint: format!("http://{}/rpc", listener.local_addr().unwrap()),
        token: "fixture".into(),
    })
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let synced = super::sync::SyncedSession::connect(http.clone())
        .await
        .unwrap();
    let request = synced.history_request("c");
    synced
        .insert_history(
            request,
            ene_rpc::History {
                items: (51..=100).map(message).collect(),
                next_cursor: Some("51:c".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    synced.refresh().await.unwrap();
    let current = synced.snapshot();
    assert_eq!(
        current.message_cursors["other"].value.as_deref(),
        Some("101:other")
    );
    assert_eq!(current.messages.len(), 100);
    // History requests use the snapshot frontier through the session transport,
    // rather than the settings mutation handler.
    http.get_history_with_options(
        ene_rpc::HistoryRequest {
            companion_id: "c".into(),
            cursor: (current.message_cursors["c"].value.as_deref()).map(Into::into),
            limit: None,
            ..Default::default()
        },
        crate::infrastructure::host::call_options(5),
    )
    .await
    .unwrap();
    assert_eq!(*observed.lock().unwrap(), Some("101:c".into()));
    synced.close().await;
    server.abort();
}

#[tokio::test]
async fn new_draft_does_not_overwrite_a_partially_saved_connection() {
    let (controller, mut receiver) = Controller::test_channel();
    let configure = |provider: &str| Command::Configure {
        settings_revision: 1,
        companion_revision: 1,
        connection_id: None,
        creation_id: None,
        provider: provider.into(),
        endpoint: format!("https://{provider}.test/v1"),
        model: "model".into(),
        options: ene_rpc::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        },
        update_connection: true,
        credential: None,
    };
    let identity = |request: &Request| match &request.command {
        Command::Configure { creation_id, .. } => creation_id.clone().unwrap(),
        _ => unreachable!(),
    };
    controller
        .request_at(
            "c",
            configure("first"),
            crate::host_model::DraftStamp {
                id: "draft".into(),
                revision: 1,
            },
        )
        .unwrap();
    let first = receiver.try_recv().unwrap();
    controller
        .request_at(
            "c",
            configure("first"),
            crate::host_model::DraftStamp {
                id: "draft".into(),
                revision: 1,
            },
        )
        .unwrap();
    assert_eq!(
        identity(&first),
        identity(&receiver.try_recv().unwrap()),
        "retry must retain its UUID"
    );
    // A partial or uncertain save has already created the first connection.
    // Choosing a different provider starts a new draft with no selected ID.
    controller
        .request_at(
            "c",
            configure("second"),
            crate::host_model::DraftStamp {
                id: "draft".into(),
                revision: 2,
            },
        )
        .unwrap();
    let second = receiver.try_recv().unwrap();
    assert_ne!(identity(&first), identity(&second));
    let mut fresh = snapshot();
    fresh.settings.get_or_insert_default().connections[0].id = identity(&first);
    fresh.settings.get_or_insert_default().connections[0].adapter = "first".into();
    fresh.settings.get_or_insert_default().connections[0].endpoint = "https://first.test/v1".into();
    let expected_id = identity(&second);
    let original = fresh.settings.connections[0].clone();
    let companion = fresh.companions[0].clone();
    let router = Router::new()
        .route("/rpc/ene.host.v1.HostService/ListProviders", post(|| async { Json(json!({"items":[{
            "id":"second","name":"Second","kind":"plugin","endpoint":"https://second.test/v1","options":{"json":"{}"},"settings":[],
            "requires_credentials":false,"environment_variable":null,"destinations":["https://second.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}
        }]})) }))
        .route("/rpc/ene.host.v1.HostService/UpdateSettings", post(move |Json(input): Json<ene_rpc::UpdateSettingsRequest>| {
            let original = original.clone(); let expected_id = expected_id.clone();
            async move {
                assert_eq!(input.connections.items.len(), 2);
                assert_eq!(input.connections.items[0].id.as_deref(), Some(original.id.as_str()));
                assert_eq!(input.connections.items[0].adapter, original.adapter);
                assert_eq!(input.connections.items[0].endpoint, original.endpoint);
                assert!(!input.connections.items[0].create);
                let created = &input.connections.items[1];
                assert_eq!(created.id.as_deref(), Some(expected_id.as_str()));
                assert!(created.create);
                Json(ene_rpc::Settings { revision: 2, connections: vec![original,
                    ene_rpc::Connection { id: expected_id, kind: created.kind.clone(),
                        adapter: created.adapter.clone(), endpoint: created.endpoint.clone(),
                        options: created.options.clone(), credential_state: "missing".into(),
                        ..Default::default() }] , ..Default::default() })
            }
        }))
        .route("/rpc/ene.host.v1.HostService/UpdateCompanion", post(move || { let companion = companion.clone(); async move { Json(companion) } }));
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
    controller
        .request_at(
            "c",
            configure("second"),
            crate::host_model::DraftStamp {
                id: "draft".into(),
                revision: 3,
            },
        )
        .unwrap();
    let replacement = receiver.try_recv().unwrap();
    assert_ne!(
        second_id,
        identity(&replacement),
        "resetting the same provider starts a new draft"
    );
}

fn context(command: Command) -> RequestContext {
    let policy = command.policy();
    RequestContext {
        companion: "c".into(),
        draft: crate::host_model::DraftStamp::default(),
        resource: policy.resource,
        saved_resource: policy.saved_resource,
        session: crate::infrastructure::host::SessionIdentity {
            host_id: "h".into(),
            epoch: "e".into(),
            subscription: "s".into(),
        },
        id: Uuid::new_v4().to_string(),
        target: policy.target.unwrap(),
    }
}
