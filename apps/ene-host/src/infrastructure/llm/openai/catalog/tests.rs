use super::*;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn catalog() -> Value {
    json!({"openai":{"models":{
        "known":{"id":"known","limit":{"context":100000,"input":90000,"output":10000},
            "modalities":{"input":["text","image"],"output":["text"]}},
        "invalid":{"limit":{"context":0,"input":-1,"output":null}}
    }},"other":{"models":{"foreign":{"limit":{"context":999999}}}}})
}

fn model(id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        metadata: None,
    }
}

#[test]
fn catalog_fills_missing_fields_without_overriding_api_or_guessing_model_ids() {
    let mut models = vec![
        model("known"),
        model("known-2026-10-01"),
        model("foreign"),
        model("invalid"),
    ];
    models[0].metadata = Some(ModelMetadata {
        source: "https://api.openai.com/v1/models".into(),
        scope: MetadataScope::Route,
        max_output_tokens: Some(512),
        input_modalities: Some(vec![]),
        supported_parameters: Some(vec!["tools".into()]),
        ..Default::default()
    });
    apply(&mut models, &catalog(), URL).unwrap();
    let info = models[0].metadata.as_ref().unwrap();
    assert_eq!(info.context_window, Some(100000));
    assert_eq!(info.max_input_tokens, Some(90000));
    assert_eq!(info.max_output_tokens, Some(512));
    assert_eq!(info.input_modalities.as_deref(), Some([].as_slice()));
    assert_eq!(
        info.supported_parameters.as_deref(),
        Some(["tools".into()].as_slice())
    );
    assert_eq!(info.field_sources["context_window"], URL);
    assert!(!info.field_sources.contains_key("max_output_tokens"));
    assert!(matches!(info.scope, MetadataScope::Catalog));
    assert!(models[1..].iter().all(|m| m.metadata.is_none()));
    // Provider data wins even when the catalog advertises a larger context window.
    let mut reported = vec![model("known")];
    reported[0].metadata = Some(ModelMetadata {
        source: "https://api.openai.com/v1/models".into(),
        scope: MetadataScope::Route,
        context_window: Some(4096),
        ..Default::default()
    });
    apply(&mut reported, &catalog(), URL).unwrap();
    let info = reported[0].metadata.as_ref().unwrap();
    assert_eq!(info.context_window, Some(4096));
    assert!(!info.field_sources.contains_key("context_window"));
}

struct AuthorizationState(Arc<AtomicBool>);
impl Authorization for AuthorizationState {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::PermissionRequired))
            }
        })
    }
}

#[tokio::test]
async fn public_catalog_request_has_no_credentials_and_rejects_redirects_failures_and_revocation() {
    let allowed = Arc::new(AtomicBool::new(true));
    let revoke = allowed.clone();
    let app = Router::new()
        .route(
            "/api.json",
            get(|headers: HeaderMap| async move {
                assert!(headers.get("authorization").is_none());
                assert!(headers.get("cookie").is_none());
                Json(catalog())
            }),
        )
        .route(
            "/redirect",
            get(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/api.json")]) }),
        )
        .route(
            "/failure",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/invalid",
            get(|| async { Json(json!({"openai":{"models":null}})) }),
        )
        .route(
            "/revoked",
            get(move || async move {
                revoke.store(false, Ordering::SeqCst);
                Json(catalog())
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let authorization = AuthorizationState(allowed);
    let mut models = vec![model("known"), model("new-model")];
    enrich_from(&mut models, &authorization, &format!("{base}/api.json"))
        .await
        .unwrap();
    assert_eq!(
        models[0].metadata.as_ref().unwrap().context_window,
        Some(100000)
    );
    assert!(models[1].metadata.is_none());
    for path in ["redirect", "failure", "invalid"] {
        let mut models = vec![model("known")];
        assert!(
            enrich_from(&mut models, &authorization, &format!("{base}/{path}"))
                .await
                .is_err()
        );
        assert!(models[0].metadata.is_none());
    }
    let mut models = vec![model("known")];
    let error = enrich_from(&mut models, &authorization, &format!("{base}/revoked"))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionRequired);
    assert!(models[0].metadata.is_none());
    server.abort();
}
