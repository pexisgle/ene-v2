//! Rig owns credentials and the wire; this boundary enforces destination and revocation.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use connectrpc::ConnectError;
use futures_util::StreamExt;
use rig_core::{
    catalog::{Catalog, ModelSpec},
    operation::Completion,
    providers::{openai::OpenAIConfig, registry::ProviderId},
    serve::{ErasedHandler, adapters::ModelAdapter},
};
use rig_ecs::{effects::Handler, model::Connection};
use rig_http::{
    http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse},
    wasm_compat::WasmCompatSend,
};
use rig_reqwest::ReqwestClient;

use super::store::Result;
use crate::proto::settings::v1::Settings;

pub(super) type Factory =
    Arc<dyn Fn(&Settings, bool, Arc<AtomicBool>) -> Result<Connection> + Send + Sync>;

pub(super) fn spec(settings: &Settings, work: bool) -> Result<Arc<ModelSpec>> {
    let model = if work {
        &settings.work_model
    } else {
        &settings.companion_model
    };
    if model.trim().is_empty() || model.len() > 200 {
        return Err(ConnectError::invalid_argument("model is required"));
    }
    let spec = match Catalog::builtin().resolve(&format!("openai/{model}")) {
        Ok(found) => found.shared(),
        Err(_) if settings.declare_tool_calling => Arc::new(
            ModelSpec::new(
                ProviderId::catalog("openai").expect("OpenAI provider"),
                model,
            )
            .with_tools(true),
        ),
        Err(_) => {
            return Err(ConnectError::failed_precondition(
                "model tool capability is unknown; an explicit declaration is required",
            ));
        }
    };
    if !spec.tools {
        return Err(ConnectError::failed_precondition("model cannot call tools"));
    }
    Ok(spec)
}

pub(super) fn validate(settings: &Settings) -> Result<()> {
    let url = url::Url::parse(&settings.endpoint)
        .map_err(|_| ConnectError::invalid_argument("invalid endpoint"))?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConnectError::invalid_argument(
            "endpoint requires HTTPS or loopback HTTP, without credentials, query or fragment",
        ));
    }
    spec(settings, false)?;
    spec(settings, true)?;
    Ok(())
}

pub(crate) fn factory() -> Result<Factory> {
    let client = ReqwestClient::from(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|_| ConnectError::unavailable("cannot build model transport"))?,
    );
    Ok(Arc::new(move |settings, work, allowed| {
        let spec = spec(settings, work)?;
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            ConnectError::failed_precondition("OPENAI_API_KEY is not configured on the host")
        })?;
        let transport = Authorized {
            client: client.clone(),
            allowed,
            destination: format!("{}/responses", settings.endpoint.trim_end_matches('/')),
        };
        let provider = OpenAIConfig::new(key)
            .with_base_url(&settings.endpoint)
            .connect(transport);
        let model = if work {
            &settings.work_model
        } else {
            &settings.companion_model
        };
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            provider.responses(model.clone()),
        ));
        Ok(Connection {
            spec,
            handler: Handler(handler),
        })
    }))
}

#[derive(Clone)]
struct Authorized {
    client: ReqwestClient,
    allowed: Arc<AtomicBool>,
    destination: String,
}

fn denied() -> http_client::Error {
    http_client::Error::instance(std::io::Error::other(
        "model request denied or transport failed",
    ))
}

impl Authorized {
    fn permits(&self, uri: &http::Uri) -> bool {
        self.allowed.load(Ordering::Acquire) && uri.to_string() == self.destination
    }
}

impl HttpClientExt for Authorized {
    fn send<T, U>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            if !this.permits(request.uri()) {
                return Err(denied());
            }
            this.client.send(request).await.map_err(|_| denied())
        }
    }
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _: http::Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        async { Err(denied()) }
    }
    fn send_streaming<T>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            if !this.permits(request.uri()) {
                return Err(denied());
            }
            let response = this
                .client
                .send_streaming(request)
                .await
                .map_err(|_| denied())?;
            let (parts, mut body) = response.into_parts();
            let bounded = async_stream::try_stream! {
                let mut size = 0usize;
                while let Some(chunk) = body.next().await {
                    if !this.allowed.load(Ordering::Acquire) { Err(denied())?; }
                    let bytes = chunk.map_err(|_| denied())?;
                    size = size.saturating_add(bytes.len());
                    if size > 8 * 1024 * 1024 { Err(denied())?; }
                    yield bytes;
                }
            };
            Ok(http::Response::from_parts(
                parts,
                Box::pin(bounded) as http_client::BoxedStream,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn provider_owns_the_credential_and_transport_errors_do_not_expose_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().route("/v1/responses", axum::routing::post(move |headers: http::HeaderMap| {
            let observed = observed.clone();
            async move {
                assert_eq!(headers[http::header::AUTHORIZATION], "Bearer fixture-only");
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    (http::StatusCode::OK, axum::Json(serde_json::json!({
                        "id":"res_fixture", "status":"completed", "model":"fixture",
                        "output":[{"type":"message", "id":"msg_fixture", "status":"completed", "role":"assistant",
                        "content":[{"type":"output_text", "text":"native credential", "annotations":[]}]}]
                    })))
                } else {
                    (http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error":{"message":"fixture-only"}})))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let allowed = Arc::new(AtomicBool::new(true));
        let config = OpenAIConfig::new("fixture-only").with_base_url(&endpoint);
        assert!(!format!("{config:?}").contains("fixture-only"));
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("fixture-only")
        );
        let model = config
            .connect(Authorized {
                client: ReqwestClient::default(),
                allowed: allowed.clone(),
                destination: format!("{endpoint}/responses"),
            })
            .responses("fixture");
        let request = || rig_core::completion::CompletionRequest::new("hello");
        assert_eq!(
            model.call(request()).await.unwrap().text(),
            "native credential"
        );
        let error = model.call(request()).await.unwrap_err();
        assert!(!format!("{error:?}").contains("fixture-only"));
        allowed.store(false, Ordering::Release);
        assert!(model.call(request()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn transport_binds_destination_secret_and_permission_on_every_send() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |headers: http::HeaderMap| {
                let observed = observed.clone();
                async move {
                    assert_eq!(
                        headers.get(http::header::AUTHORIZATION).unwrap(),
                        "Bearer fixture-only"
                    );
                    observed.fetch_add(1, Ordering::SeqCst);
                    "response body"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let allowed = Arc::new(AtomicBool::new(true));
        let transport = Authorized {
            client: ReqwestClient::from(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
            ),
            allowed: allowed.clone(),
            destination: destination.clone(),
        };
        let request = |uri: &str| {
            http::Request::post(uri)
                .header(http::header::AUTHORIZATION, "Bearer fixture-only")
                .body(Bytes::new())
                .unwrap()
        };
        assert!(
            transport
                .send_streaming(request(&format!("{destination}/different")))
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let response = transport
            .send_streaming(request(&destination))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        allowed.store(false, Ordering::Release);
        let mut body = response.into_body();
        assert!(body.next().await.unwrap().is_err());
        assert!(
            transport
                .send_streaming(request(&destination))
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn model_listing_is_not_a_capability_or_destination_authorization() {
        let mut settings = Settings {
            endpoint: "https://api.openai.com/v1".into(),
            companion_model: "not-in-catalog".into(),
            work_model: "not-in-catalog".into(),
            ..Default::default()
        };
        assert!(validate(&settings).is_err());
        settings.declare_tool_calling = true;
        validate(&settings).unwrap();
        settings.endpoint = "http://example.com/v1".into();
        assert!(validate(&settings).is_err());
        settings.endpoint = "https://user:secret@example.com/v1".into();
        assert!(validate(&settings).is_err());
        settings.endpoint = "http://127.0.0.1:9999/v1".into();
        validate(&settings).unwrap();
    }
}
