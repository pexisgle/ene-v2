use super::*;
use crate::infrastructure::sqlite::Store;
use axum::http::{StatusCode, header};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;
async fn app() -> (App, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    (
        crate::bootstrap::application(store, "private-token".into()),
        temp,
    )
}
async fn request(app: &App, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, token);
    }
    let response = router(app.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn all_endpoints_require_bearer_without_echoing_secrets() {
    let (app, _temp) = app().await;
    for path in ["/v1/snapshot", "/v1/settings", "/v1/events", "/missing"] {
        for token in [
            None,
            Some("Bearer wrong"),
            Some("private-token"),
            Some("Bearer private-token "),
        ] {
            let (status, body) = request(&app, path, token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(body["error"]["code"], "unauthorized");
            assert!(!body.to_string().contains("private-token"));
        }
    }
}
#[tokio::test]
async fn snapshot_subscription_validation_and_truthful_capabilities() {
    let (app, _temp) = app().await;
    let (status, body) = request(&app, "/v1/snapshot", Some("Bearer private-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["capabilities"]["features"],
        json!([
            "history",
            "notifications",
            "chat",
            "connection_settings",
            "credentials",
            "data_transfer_grants",
            "connection_check"
        ])
    );
    assert_eq!(
        body["companions"][0]["availability"]["chat"]["ready"],
        false
    );
    assert!(
        body["settings"]["connections"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (status, _) = request(
        &app,
        "/v1/snapshot?connection_id=missing",
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let subscription = app.application.subscribe().await;
    let (status, _) = request(
        &app,
        &format!("/v1/snapshot?connection_id={}", subscription.id),
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app.application.unsubscribe(&subscription.id).await;
    let (status, _) = request(
        &app,
        &format!("/v1/snapshot?connection_id={}", subscription.id),
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
#[tokio::test]
async fn history_rejects_foreign_ids_bad_cursors_and_unknown_query_fields() {
    let (app, _temp) = app().await;
    let c = app
        .application
        .snapshot(None)
        .await
        .unwrap()
        .stored
        .companion
        .id;
    for suffix in [
        "limit=0",
        "limit=201",
        "limit=oops",
        "cursor=1:other",
        "cursor=oops",
        "unexpected=1",
    ] {
        let (status, _) = request(
            &app,
            &format!("/v1/companions/{c}/messages?{suffix}"),
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, body) = request(
        &app,
        &format!("/v1/companions/{c}/messages"),
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    let (status, _) = request(
        &app,
        "/v1/companions/other/messages",
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn mutate(
    app: &App,
    method: &str,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer private-token")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        req = req.header("Idempotency-Key", key);
    }
    let response = router(app.clone())
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}
#[tokio::test]
async fn permissions_are_independent_and_revocation_does_not_wait_for_generation() {
    use crate::{
        conversation as db,
        service::{Running, Secret},
    };
    let (app, _temp) = app().await;
    let (status,saved)=mutate(&app,"PATCH","/v1/settings",json!({"revision":1,"connections":[{"kind":"llm","adapter":"openrouter","endpoint":crate::service::ENDPOINT,"options":{"only":["openai"],"allow_fallbacks":false}}]}),None).await;
    assert_eq!(status, StatusCode::OK);
    let p = saved["connections"][0]["id"].as_str().unwrap();
    let c = app.store.snapshot().await.unwrap().companion;
    let(status,_)=mutate(&app,"PATCH",&format!("/v1/companions/{}",c.id),json!({"revision":1,"model_assignments":{"companion":{"connection_id":p,"model_id":crate::service::MODEL}}}),None).await;
    assert_eq!(status, StatusCode::OK);
    {
        let mut controls = app.controls.lock().await;
        controls.secrets.insert(
            p.into(),
            Secret {
                value: "test-secret-never-sent".into(),
                nonce: "test".into(),
            },
        );
        let mut config = controls.config.clone().unwrap();
        let mut connection = db::connection(&config, p).unwrap();
        connection.credential_state = "session_only".into();
        crate::service::update_connection(&mut config, connection);
        crate::service::commit(&app, &mut controls, config)
            .await
            .unwrap();
    }
    let message_path = format!("/v1/companions/{}/messages", c.id);
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"未許可"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let grant = json!({"companion_id":c.id,"connection_id":p,"kind":"data_transfer","destinations":["https://openrouter.ai","openai"],"data_kinds":["test_input"],"purpose":"connection_check"});
    assert_eq!(
        mutate(&app, "POST", "/v1/grants", grant, Some("check-grant"))
            .await
            .0,
        StatusCode::CREATED
    );
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"疎通許可は会話許可を代用しない"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let grant = json!({"companion_id":c.id,"connection_id":p,"kind":"data_transfer","destinations":["https://openrouter.ai","openai"],"data_kinds":["conversation"],"purpose":"conversation"});
    let (status, receipt) = mutate(
        &app,
        "POST",
        "/v1/grants",
        grant.clone(),
        Some("chat-grant"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        mutate(&app, "POST", "/v1/grants", grant, Some("chat-grant"))
            .await
            .1,
        receipt
    );
    let (cancel, mut receiver) = tokio::sync::watch::channel(false);
    app.controls.lock().await.running.insert(
        "fake-generation".into(),
        Running {
            connection: p.into(),
            purpose: "conversation",
            cancel,
        },
    );
    let check_id = app
        .controls
        .lock()
        .await
        .config
        .as_ref()
        .unwrap()
        .grants
        .iter()
        .find(|g| g.purpose == "connection_check")
        .unwrap()
        .id
        .clone();
    let (check_cancel, check_receiver) = tokio::sync::watch::channel(false);
    app.controls.lock().await.running.insert(
        "fake-check".into(),
        Running {
            connection: p.into(),
            purpose: "connection_check",
            cancel: check_cancel,
        },
    );
    let (status, _) = mutate(
        &app,
        "DELETE",
        &format!("/v1/grants/{check_id}"),
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(*check_receiver.borrow());
    assert!(!*receiver.borrow());
    let (status, _) = mutate(
        &app,
        "DELETE",
        &format!("/v1/grants/{}", receipt["grant"]["id"].as_str().unwrap()),
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(*receiver.borrow_and_update());
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"解除後"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, snapshot) = request(&app, "/v1/snapshot", Some("Bearer private-token")).await;
    assert!(!snapshot.to_string().contains("test-secret-never-sent"));
    assert_eq!(
        snapshot["companions"][0]["availability"]["chat"]["ready"],
        false
    );
}
#[tokio::test]
async fn concurrent_acceptance_replays_one_request_and_never_sends_without_controls() {
    let (app, _temp) = app().await;
    // Existing durable acceptance must replay even when credentials/grants are absent.
    let c = app.store.snapshot().await.unwrap().companion.id;
    let id = c.clone();
    app.store
        .run(move |db| {
            crate::conversation::accept(
                db,
                "replay",
                &id,
                json!({"text":"日本🙂","resources":[],"question_id":null,"decision":null}),
            )
        })
        .await
        .unwrap();
    let path = format!("/v1/companions/{c}/messages");
    let (a, b) = tokio::join!(
        mutate(
            &app,
            "POST",
            &path,
            json!({"text":"日本🙂"}),
            Some("replay")
        ),
        mutate(
            &app,
            "POST",
            &path,
            json!({"text":"日本🙂"}),
            Some("replay")
        )
    );
    assert_eq!(a.0, StatusCode::ACCEPTED);
    assert_eq!(a, b);
    assert!(app.controls.lock().await.running.is_empty());
    assert_eq!(
        mutate(&app, "POST", &path, json!({"text":"変更"}), Some("replay"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.store.messages(c, None, 50).await.unwrap().items.len(),
        2
    );
}
