use super::*;
use ene_core::ports::llm::{Authorization, test_support::InvocationFixture};
use futures_util::future::BoxFuture;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Authority(Arc<AtomicBool>);
impl Authorization for Authority {
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
fn models() -> Vec<Model> {
    vec![Model {
        id: "gpt-model".into(),
        name: "Provider model".into(),
        metadata: Some(ModelMetadata {
            source: "https://api.openai.com/v1/models".into(),
            scope: MetadataScope::Route,
            ..Default::default()
        }),
    }]
}

#[tokio::test]
async fn optional_catalog_failures_keep_provider_models_but_revocation_stops_reads() {
    for (status, body, revoke) in [
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            false,
        ),
        (axum::http::StatusCode::OK, "invalid json", false),
        (axum::http::StatusCode::OK, "{}", false),
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            true,
        ),
    ] {
        let permitted = Arc::new(AtomicBool::new(true));
        let arrived = Arc::new(AtomicBool::new(false));
        let p = permitted.clone();
        let a = arrived.clone();
        let router = axum::Router::new().route(
            "/catalog",
            axum::routing::get(move || {
                let p = p.clone();
                let a = a.clone();
                async move {
                    a.store(true, Ordering::SeqCst);
                    if revoke {
                        p.store(false, Ordering::SeqCst);
                    }
                    (status, body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = format!("http://{}/catalog", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut models = models();
        let before = serde_json::to_value(&models).unwrap();
        let connection = ene_core::domain::Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: "openai".into(),
            endpoint: "https://api.openai.com/v1".into(),
            options: serde_json::json!({}),
            credential_state: "missing".into(),
            capabilities: None,
            last_check: None,
        };
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let invocation = InvocationFixture {
            id: "m".into(),
            connection,
            request: None,
            secret: None,
            authorization: Arc::new(Authority(permitted)),
            cancel: receiver,
            operation: ene_core::domain::OperationKind::ModelList,
        }
        .with_destinations(vec![
            url::Url::parse(&source)
                .unwrap()
                .origin()
                .ascii_serialization(),
        ]);
        let result = optional_enrich(&mut models, Arc::new(invocation), &source).await;
        assert!(arrived.load(Ordering::SeqCst));
        if revoke {
            assert_eq!(result.unwrap_err().kind, ErrorKind::PermissionRequired);
        } else {
            result.unwrap();
        }
        assert_eq!(serde_json::to_value(&models).unwrap(), before);
        server.abort();
    }
}
