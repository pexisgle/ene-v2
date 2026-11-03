use super::*;
use rig_http::http_client::{HttpClientExt, LazyBody, MultipartForm, StreamingResponse};
use rig_http::wasm_compat::WasmCompatSend;

impl HttpClientExt for AuthorizedTransport {
    fn send<T, U>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let transport = self.clone();
        let request = request.map(Into::into);
        async move {
            let response = transport.send(request).await.map_err(rig_error)?;
            let mut builder = http::Response::builder().status(response.status());
            *builder.headers_mut().expect("response headers") = response.headers().clone();
            let body: LazyBody<U> =
                Box::pin(async move { response.bytes().await.map(U::from).map_err(rig_error) });
            Ok(builder.body(body)?)
        }
    }
    // The Rig contract requires a 'static future; async fn captures &self.
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _: http::Request<MultipartForm>,
    ) -> impl Future<Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        async { Err(rig_error(Error::new(ErrorKind::UnsupportedCapability))) }
    }
    fn send_streaming<T>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = rig_http::http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let transport = self.clone();
        let request = request.map(Into::into);
        async move {
            let mut response = transport.send(request).await.map_err(rig_error)?;
            let mut builder = http::Response::builder().status(response.status());
            *builder.headers_mut().expect("response headers") = response.headers().clone();
            let body: rig_http::http_client::BoxedStream = Box::pin(async_stream::try_stream! {
                let mut framing = EventBudget::default();
                while let Some(bytes) = response.chunk().await.map_err(rig_error)? {
                    framing.accept(&bytes).map_err(rig_error)?;
                    yield bytes;
                }
            });
            Ok(builder.body(body)?)
        }
    }
}
fn rig_error(error: Error) -> rig_http::http_client::Error {
    rig_http::http_client::Error::instance(error)
}

const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Tracks framing across HTTP chunks before Rig can buffer an oversized event.
#[derive(Default)]
struct EventBudget {
    event: usize,
    line_content: bool,
}
impl EventBudget {
    fn accept(&mut self, bytes: &[u8]) -> Result<()> {
        for byte in bytes {
            self.event += 1;
            if self.event > MAX_EVENT_BYTES {
                return Err(external());
            }
            if *byte == b'\n' {
                if !self.line_content {
                    self.event = 0;
                }
                self.line_content = false;
            } else if *byte != b'\r' {
                self.line_content = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Connection, OperationKind},
        llm::{Authorization, test_support::InvocationFixture},
        ports::Secret,
    };
    use axum::{
        Router,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse,
        routing::get,
    };
    use futures_util::{StreamExt, future::BoxFuture};
    use rig_http::http_client::HttpClientExt;
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    struct Gate(AtomicBool);
    impl Authorization for Gate {
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
    fn transport(
        endpoint: String,
        gate: Arc<Gate>,
        operation: OperationKind,
        placement: CredentialPlacement,
    ) -> AuthorizedTransport {
        let origin = url::Url::parse(&endpoint)
            .unwrap()
            .origin()
            .ascii_serialization();
        let (_, cancel) = tokio::sync::watch::channel(false);
        let invocation = InvocationFixture {
            id: "test".into(),
            connection: Connection {
                id: "c".into(),
                kind: "llm".into(),
                adapter: "test".into(),
                endpoint,
                options: ene_rpc::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                }
                .into(),
                credential_state: "stored".into(),
                capabilities: Default::default(),
                last_check: Default::default(),
                ..Default::default()
            },
            request: None,
            secret: Some(Secret {
                value: "hidden-secret".into(),
                nonce: "n".into(),
            }),
            authorization: gate,
            cancel,
            operation,
        }
        .with_destinations(vec![origin]);
        AuthorizedTransport::new(Arc::new(invocation), placement).unwrap()
    }
    async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (origin, task)
    }
    fn request(url: String) -> http::Request<Bytes> {
        http::Request::builder()
            .uri(url)
            .body(Bytes::new())
            .unwrap()
    }
    fn post(url: String) -> http::Request<Bytes> {
        http::Request::builder()
            .method("POST")
            .uri(url)
            .body(Bytes::new())
            .unwrap()
    }

    #[tokio::test]
    async fn redirects_never_forward_credentials_and_query_failures_are_redacted() {
        let reached = Arc::new(AtomicBool::new(false));
        let sink = reached.clone();
        let (origin, task) = server(
            Router::new()
                .route(
                    "/api/models",
                    get(|uri: Uri, headers: HeaderMap| async move {
                        assert!(headers.get("authorization").is_none());
                        assert_eq!(
                            url::form_urlencoded::parse(uri.query().unwrap().as_bytes())
                                .next()
                                .unwrap()
                                .1,
                            "hidden-secret"
                        );
                        (StatusCode::TEMPORARY_REDIRECT, [("location", "/sink")]).into_response()
                    }),
                )
                .route(
                    "/sink",
                    get(move || async move {
                        sink.store(true, Ordering::SeqCst);
                        "leaked"
                    }),
                ),
        )
        .await;
        let gate = Arc::new(Gate(AtomicBool::new(true)));
        let transport = transport(
            format!("{origin}/api"),
            gate,
            OperationKind::ModelList,
            CredentialPlacement::Query("key".into()),
        );
        let error = transport
            .send(request(format!("{origin}/api/models")))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::PermissionRequired);
        assert!(!format!("{error:?}").contains("hidden-secret"));
        assert!(!reached.load(Ordering::SeqCst));
        let error = transport
            .send(request(format!("{origin}/api/models?key=injected")))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::PermissionRequired);
        task.abort();
    }

    #[tokio::test]
    async fn lazy_send_and_body_receiving_recheck_authority() {
        let arrived = Arc::new(AtomicBool::new(false));
        let saw = arrived.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let body_release = release.clone();
        let (origin, task) = server(Router::new().route(
            "/api/models",
            get(move |headers: HeaderMap| {
                let saw = saw.clone();
                let release = body_release.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer hidden-secret");
                    saw.store(true, Ordering::SeqCst);
                    axum::body::Body::from_stream(async_stream::stream! {
                        release.notified().await;
                        yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"body"));
                    })
                }
            }),
        ))
        .await;
        let gate = Arc::new(Gate(AtomicBool::new(true)));
        let transport = transport(
            format!("{origin}/api"),
            gate.clone(),
            OperationKind::ModelList,
            CredentialPlacement::Bearer,
        );
        let lazy =
            HttpClientExt::send::<_, Bytes>(&transport, request(format!("{origin}/api/models")));
        gate.0.store(false, Ordering::SeqCst);
        assert!(lazy.await.is_err());
        assert!(!arrived.load(Ordering::SeqCst));
        gate.0.store(true, Ordering::SeqCst);
        let mut response = transport
            .send(request(format!("{origin}/api/models")))
            .await
            .unwrap();
        let reading = response.chunk();
        tokio::pin!(reading);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut reading)
                .await
                .is_err()
        );
        gate.0.store(false, Ordering::SeqCst);
        release.notify_one();
        assert_eq!(
            reading.await.unwrap_err().kind,
            ErrorKind::PermissionRequired
        );
        task.abort();
    }

    #[tokio::test]
    async fn an_oversized_response_body_fails_instead_of_reading_forever() {
        let (origin, task) = server(Router::new().fallback(axum::routing::post(|| async {
            "x".repeat(MAX_RESPONSE_BYTES + 1)
        })))
        .await;
        let transport = transport(
            format!("{origin}/v1"),
            Arc::new(Gate(AtomicBool::new(true))),
            OperationKind::Conversation,
            CredentialPlacement::None,
        );
        // A declared length over the budget is refused before any body is
        // read; a chunked body is stopped as it grows.
        if let Ok(response) = transport
            .send_streaming(post(format!("{origin}/v1/chat/completions")))
            .await
        {
            let mut body = response.into_body();
            let mut read = 0usize;
            let failed = loop {
                match body.next().await {
                    Some(Ok(chunk)) => read += chunk.len(),
                    Some(Err(_)) => break true,
                    None => break false,
                }
            };
            assert!(failed, "the oversized body was read whole: {read} bytes");
        }
        task.abort();
    }

    #[tokio::test]
    async fn chunked_wire_and_sse_budgets_apply_before_adapter_parsing() {
        let (origin, task) = server(
            Router::new()
                .route(
                    "/api/chat/completions",
                    axum::routing::post(|| async {
                        axum::body::Body::from_stream(futures_util::stream::iter((0..9).map(
                            |_| {
                                Ok::<_, std::convert::Infallible>(Bytes::from(vec![
                                    b'x';
                                    1024 * 1024
                                ]))
                            },
                        )))
                    }),
                )
                .route(
                    "/api/responses",
                    axum::routing::post(|| async {
                        axum::body::Body::from_stream(futures_util::stream::iter((0..17).map(
                            |_| {
                                Ok::<_, std::convert::Infallible>(Bytes::from(vec![
                                    b'x';
                                    64 * 1024
                                ]))
                            },
                        )))
                    }),
                ),
        )
        .await;
        let transport = transport(
            format!("{origin}/api"),
            Arc::new(Gate(AtomicBool::new(true))),
            OperationKind::Conversation,
            CredentialPlacement::Bearer,
        );
        assert_eq!(
            transport
                .send(post(format!("{origin}/api/chat/completions")))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap_err()
                .kind,
            ErrorKind::ProviderFailure
        );
        let response = transport
            .send_streaming(post(format!("{origin}/api/responses")))
            .await
            .unwrap();
        let mut body = response.into_body();
        let mut failed = false;
        while let Some(chunk) = body.next().await {
            if chunk.is_err() {
                failed = true;
                break;
            }
        }
        assert!(
            failed,
            "oversized unterminated SSE event reached the parser"
        );
        task.abort();
    }
    #[test]
    fn sse_boundaries_survive_chunk_splits() {
        let mut budget = EventBudget::default();
        budget.accept(&vec![b'x'; MAX_EVENT_BYTES - 2]).unwrap();
        budget.accept(b"\n").unwrap();
        budget.accept(b"\n").unwrap();
        budget.accept(&vec![b'x'; MAX_EVENT_BYTES]).unwrap();
        assert!(budget.accept(b"x").is_err());
    }
}
