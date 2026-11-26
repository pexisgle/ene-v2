use super::super::{
    error::{HostError, Result},
    net,
    settings::Endpoint,
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use http::{HeaderValue, Method, Request, Response, StatusCode, Uri};
use rig_core::{
    catalog::{Catalog, ModelFacts, ModelSpec},
    driver::{DynModel, Model},
    http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse},
    operation::Completion,
    providers::{
        gemini::{GeminiConfig, completion::GenerateContent},
        registry::ProviderId,
    },
};
use std::{collections::HashSet, sync::Arc, time::Duration};

pub(super) use ene_catalog::GOOGLE_AGENT_PLATFORM_DEFAULT_MODEL as DEFAULT_MODEL;
pub(crate) use ene_catalog::{
    GOOGLE_AGENT_PLATFORM as ADAPTER, GOOGLE_AGENT_PLATFORM_ENDPOINT as ENDPOINT,
    GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST as VERIFIED_MODEL_LIST,
};

// Google Cloud's Gemini chat model IDs. Use the same candidates for both
// connection modes and let Google decide what each key and location can use.
const CHAT_MODELS: &[&str] = &[
    DEFAULT_MODEL,
    "gemini-4-neon",
    "gemini-4-hydrogen",
    "gemini-4-carbon",
    "gemini-4-argon",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools",
    "gemini-3-flash-preview",
];

pub(crate) fn valid_model(id: &str) -> bool {
    ene_domain::settings::is_valid_agent_platform_model(id)
}

fn publisher_endpoint(endpoint: &Endpoint) -> super::super::error::Result<String> {
    ene_domain::settings::agent_platform_publisher_endpoint(endpoint).map_err(Into::into)
}

fn api_key(secret: Option<&str>) -> Result<HeaderValue> {
    let secret = secret
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            HostError::FailedPrecondition("Google Agent Platform の API キーが未登録です".into())
        })?;
    let mut key = HeaderValue::from_str(secret).map_err(|_| {
        HostError::Invalid("Google Agent Platform の API キーの形式が無効です".into())
    })?;
    key.set_sensitive(true);
    Ok(key)
}

#[derive(Debug, thiserror::Error)]
pub(super) enum AgentPlatformError {
    #[error("{0}")]
    Host(#[from] HostError),
    #[error(transparent)]
    Net(#[from] net::NetError),
    #[error("Google Agent Platform のモデル確認がタイムアウトしました。一覧を再取得してください")]
    ProbeTimeout,
    #[error("Google Agent Platform で利用できるモデルを確認できませんでした")]
    NoModelsVerified,
    #[error("Google Agent Platform のモデルを確認できませんでした")]
    CountTokensRequestFailed,
    #[error(
        "Google Agent Platform がリクエストを拒否しました。API キー、プロジェクトと権限を確認してください"
    )]
    RequestRejected,
    #[error("Google Agent Platform のモデル確認に失敗しました（HTTP {0}）")]
    UnexpectedStatus(StatusCode),
    #[error("Google Agent Platform のトークン数を取得できませんでした")]
    InvalidTokenCount,
    #[error("Google Agent Platform で {0} をこのロケーションでは利用できません")]
    ModelUnavailable(String),
}

pub(super) async fn verify_models(
    endpoint: &Endpoint,
    secret: &str,
    configured: &[String],
) -> std::result::Result<VerifiedModels, AgentPlatformError> {
    let publisher = publisher_endpoint(endpoint)?;
    let key = api_key(Some(secret))?;
    verify_candidates(&net::client()?, &publisher, &key, configured).await
}

pub(super) struct VerifiedModels {
    pub(super) ids: Vec<String>,
    pub(super) failure: Option<String>,
}

async fn verify_candidates(
    client: &reqwest::Client,
    publisher: &str,
    key: &HeaderValue,
    configured: &[String],
) -> std::result::Result<VerifiedModels, AgentPlatformError> {
    let selected = configured.first().map(|id| id.trim());
    let mut seen = HashSet::new();
    // Check the selected model first, then every known chat candidate, then
    // any other manually configured IDs. Each ID goes to Google only once.
    let candidates: Vec<String> = selected
        .into_iter()
        .chain(CHAT_MODELS.iter().copied())
        .chain(configured.iter().map(|id| id.trim()))
        .filter(|&id| valid_model(id) && seen.insert(id))
        .map(str::to_owned)
        .collect();
    let requests = stream::iter(candidates)
        .map(move |id| async move {
            let result = count_tokens(client, publisher, key, &id).await;
            (id, result)
        })
        .buffered(5);
    futures_util::pin_mut!(requests);
    let deadline = tokio::time::Instant::now() + super::PROBE_TIMEOUT;
    let mut ids = Vec::new();
    let mut failure: Option<AgentPlatformError> = None;
    loop {
        match tokio::time::timeout_at(deadline, requests.next()).await {
            Ok(Some((id, Ok(true)))) => ids.push(id),
            Ok(Some((id, Ok(false)))) => {
                if selected == Some(id.as_str()) {
                    failure = Some(AgentPlatformError::ModelUnavailable(id));
                }
            }
            Ok(Some((id, Err(error)))) => {
                if selected == Some(id.as_str()) {
                    failure = Some(error);
                } else {
                    failure.get_or_insert(error);
                }
            }
            Ok(None) => break,
            Err(_) => {
                failure.get_or_insert(AgentPlatformError::ProbeTimeout);
                break;
            }
        }
    }
    if ids.is_empty() {
        return Err(failure.unwrap_or(AgentPlatformError::NoModelsVerified));
    }
    Ok(VerifiedModels {
        ids,
        failure: failure.map(|error| error.to_string()),
    })
}

async fn count_tokens(
    client: &reqwest::Client,
    publisher: &str,
    key: &HeaderValue,
    model: &str,
) -> std::result::Result<bool, AgentPlatformError> {
    // ListPublisherModels requires OAuth. CountTokens accepts the connection's
    // API key and checks the exact model and location without generating text.
    let response = client
        .post(format!("{publisher}/{model}:countTokens"))
        .header("x-goog-api-key", key.clone())
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(r#"{"contents":[{"role":"user","parts":[{"text":"Hi"}]}]}"#)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|_| AgentPlatformError::CountTokensRequestFailed)?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(match status {
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                AgentPlatformError::RequestRejected
            }
            _ => AgentPlatformError::UnexpectedStatus(status),
        });
    }
    let body =
        net::read_response_body(response, 16 * 1024, "Google Agent Platform token count").await?;
    let result: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| AgentPlatformError::InvalidTokenCount)?;
    if result["totalTokens"].as_u64().is_none() {
        return Err(AgentPlatformError::InvalidTokenCount);
    }
    Ok(true)
}

pub(super) fn model(
    endpoint: &Endpoint,
    secret: Option<&str>,
    spec: &ModelSpec,
    catalog: &Catalog,
) -> super::super::error::Result<DynModel<Completion>> {
    let client = net::model_client().map_err(|error| HostError::Unavailable(error.to_string()))?;
    model_with_http(
        endpoint,
        secret,
        spec,
        catalog,
        rig_reqwest::ReqwestClient::from(client),
    )
}

fn model_with_http(
    endpoint: &Endpoint,
    secret: Option<&str>,
    spec: &ModelSpec,
    catalog: &Catalog,
    http: impl HttpClientExt + Clone + 'static,
) -> super::super::error::Result<DynModel<Completion>> {
    let publisher = publisher_endpoint(endpoint)?;
    if !valid_model(&spec.id) {
        return Err(HostError::Invalid(
            "Invalid Agent Platform Gemini model ID".into(),
        ));
    }
    let key = api_key(secret)?;
    // `GenerateContent` reports descriptor vendor `"gcp.gemini"`, while the
    // incoming spec is resolved under `"vertexai"` (a catalog-only vendor).
    // `ModelFacts::for_model` returns a bound spec only when its provider is
    // declared or matches the wire's descriptor vendor, so bind under the
    // declared adapter vendor while keeping all catalog facts intact.
    let mut bound_spec = spec.clone();
    bound_spec.provider = ProviderId::declare(ADAPTER)
        .map_err(|error| HostError::Internal(format!("invalid adapter name: {error}")))?;
    let wire = GenerateContent::new(
        GeminiConfig::new("").with_base_url(ENDPOINT),
        &bound_spec.id,
    )
    .with_facts(ModelFacts::new(Arc::new(bound_spec)).with_catalog(catalog));
    Ok(Model::new(
        wire,
        AgentPlatformHttp {
            http,
            publisher,
            key,
        },
    )
    .erase())
}

#[derive(Clone)]
struct AgentPlatformHttp<C> {
    http: C,
    publisher: String,
    key: HeaderValue,
}

impl<C> AgentPlatformHttp<C> {
    fn route<T>(&self, mut request: Request<T>) -> Option<Request<T>> {
        // Rig encodes Gemini Developer API paths. Use only its model and verb,
        // and construct the HTTPS destination before adding the Cloud key.
        let target = request
            .uri()
            .path()
            .strip_prefix("/v1beta/models/")
            .and_then(|path| path.split_once(':'))
            .filter(|(id, _)| valid_model(id));
        let (model, suffix) = match target {
            Some((id, "generateContent")) => (id, "generateContent"),
            Some((id, "streamGenerateContent")) => (id, "streamGenerateContent?alt=sse"),
            _ => return None,
        };
        if request.method() != Method::POST {
            return None;
        }
        let uri = format!("{}/{model}:{suffix}", self.publisher)
            .parse::<Uri>()
            .ok()?;
        *request.uri_mut() = uri;
        request
            .headers_mut()
            .insert("x-goog-api-key", self.key.clone());
        Some(request)
    }
}

fn unsupported_request() -> http_client::Error {
    http_client::Error::Instance(Box::new(std::io::Error::other(
        "unsupported Agent Platform request",
    )))
}

impl<C: HttpClientExt> HttpClientExt for AgentPlatformHttp<C> {
    fn send<T: Into<Bytes> + Send, U: From<Bytes> + Send + 'static>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static {
        let sent = self.route(req).map(|req| self.http.send(req));
        async move { sent.ok_or_else(unsupported_request)?.await }
    }

    fn send_multipart<U: From<Bytes> + Send + 'static>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static {
        std::future::ready(Err(unsupported_request()))
    }

    async fn send_streaming<T: Into<Bytes> + Send>(
        &self,
        req: Request<T>,
    ) -> http_client::Result<StreamingResponse> {
        self.http
            .send_streaming(self.route(req).ok_or_else(unsupported_request)?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::{
        catalog::Catalog,
        completion::{CompletionRequest, ToolDefinition},
        message::{Message, ToolName, ToolResultContent, UserContent},
        test_utils::{MockHttpResponse, SequencedHttpClient},
    };
    use serde_json::{Value, json};

    fn spec() -> Arc<ModelSpec> {
        Catalog::builtin()
            .resolve(&format!("vertexai/{DEFAULT_MODEL}"))
            .unwrap()
            .shared()
    }

    #[test]
    fn derives_google_publisher_paths_and_rejects_invalid_settings() {
        for (options, expected) in [
            ("{}", format!("{ENDPOINT}/v1/publishers/google/models")),
            (
                r#"{"project_id":"my-project"}"#,
                format!("{ENDPOINT}/v1/projects/my-project/locations/global/publishers/google/models"),
            ),
            (
                r#"{"project_id":"my-project","location":"asia-northeast1"}"#,
                "https://asia-northeast1-aiplatform.googleapis.com/v1/projects/my-project/locations/asia-northeast1/publishers/google/models".into(),
            ),
            (
                r#"{"project_id":"my-project","location":"eu"}"#,
                "https://aiplatform.eu.rep.googleapis.com/v1/projects/my-project/locations/eu/publishers/google/models".into(),
            ),
        ] {
            let endpoint = Endpoint { options: options.into(), ..Default::default() };
            assert_eq!(publisher_endpoint(&endpoint).unwrap(), expected);
        }
        for options in [
            r#"{"location":"global"}"#,
            r#"{"project_id":"my/project"}"#,
            r#"{"project_id":"my-project","location":"bad.example"}"#,
            r#"{"project_id":123}"#,
        ] {
            let endpoint = Endpoint {
                options: options.into(),
                ..Default::default()
            };
            assert!(publisher_endpoint(&endpoint).is_err());
        }
        let endpoint = Endpoint {
            endpoint: "https://example.org".into(),
            ..Default::default()
        };
        assert!(publisher_endpoint(&endpoint).is_err());
        assert!(!valid_model("gemini-foo/../../bar"));
    }

    #[tokio::test]
    async fn checks_each_candidate_with_the_registered_key_and_excludes_missing_models() {
        let app = axum::Router::new().route(
            "/v1/projects/my-project/locations/global/publishers/google/models/{call}",
            axum::routing::post(
                |axum::extract::Path(call): axum::extract::Path<String>,
                 headers: http::HeaderMap,
                 body: String| async move {
                    assert!(body.contains("\"parts\""));
                    if headers["x-goog-api-key"] != "test-key" {
                        return (StatusCode::UNAUTHORIZED, r#"{"error":"secret-rejected"}"#);
                    }
                    match call.as_str() {
                        "gemini-3.8-flash:countTokens"
                        | "gemini-4-neon:countTokens"
                        | "gemini-custom-model:countTokens" => {
                            (StatusCode::OK, r#"{"totalTokens":1}"#)
                        }
                        _ => (StatusCode::NOT_FOUND, "{}"),
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/projects/my-project/locations/global/publishers/google/models",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = net::client().unwrap();
        let models = verify_candidates(
            &http,
            &url,
            &api_key(Some("test-key")).unwrap(),
            &[
                "gemini-missing".into(),
                "gemini-custom-model".into(),
                "gemini-4-neon".into(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            models.ids,
            ["gemini-3.8-flash", "gemini-4-neon", "gemini-custom-model"]
        );
        assert!(models.failure.unwrap().contains("gemini-missing"));
        let rejected =
            verify_candidates(&http, &url, &api_key(Some("wrong-key")).unwrap(), &[]).await;
        let Err(error) = rejected else {
            panic!("credentials should be rejected");
        };
        let message = error.to_string();
        assert!(message.contains("API キー"));
        assert!(!message.contains("secret-rejected"));
        server.abort();
    }

    #[tokio::test]
    async fn streams_a_tool_call_and_replays_its_signature_and_result() {
        let tool_call = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{
                    "functionCall": {"name": "lookup", "args": {"q": 1}, "id": "call-a"},
                    "thoughtSignature": "c2ln"
                }]},
                "finishReason": "STOP"
            }]
        });
        let reply = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "OK"}]},
                "finishReason": "STOP"
            }]
        });
        let http = SequencedHttpClient::new([
            MockHttpResponse::success_typed(format!("data: {tool_call}\n\n"), "text/event-stream"),
            MockHttpResponse::success(reply.to_string()),
        ]);
        let model = model_with_http(
            &Endpoint::default(),
            Some("test-key"),
            &spec(),
            Catalog::builtin(),
            http.clone(),
        )
        .unwrap();
        let bound_spec = model
            .spec()
            .expect("Agent Platform completion model must expose catalog facts");
        assert_eq!(bound_spec.id, DEFAULT_MODEL);
        assert_eq!(bound_spec.provider.vendor(), ADAPTER);
        assert!(bound_spec.context_window.is_some());
        assert!(bound_spec.tools);
        let tool = ToolDefinition {
            name: ToolName::new("lookup").unwrap(),
            description: "Lookup test".into(),
            parameters: json!({"type":"object"}),
            annotations: Default::default(),
        };
        let mut request = CompletionRequest::new("Look it up");
        request.tools.push(tool.clone().into());
        let response = model.stream(request).unwrap().finish().await.unwrap();
        let call = response.tool_calls().next().unwrap();
        let mut follow_up = CompletionRequest::from(vec![
            Message::user("Look it up"),
            response.message().unwrap(),
            Message::User {
                content: vec![UserContent::ToolResult(
                    call.result(vec![ToolResultContent::text("found")]),
                )],
            },
        ]);
        follow_up.tools.push(tool.into());
        model.call(follow_up).await.unwrap();

        let requests = http.requests();
        assert_eq!(
            requests[0].uri,
            format!(
                "{ENDPOINT}/v1/publishers/google/models/{DEFAULT_MODEL}:streamGenerateContent?alt=sse"
            )
        );
        assert_eq!(
            requests[1].uri,
            format!("{ENDPOINT}/v1/publishers/google/models/{DEFAULT_MODEL}:generateContent")
        );
        for request in &requests {
            let key = &request.headers["x-goog-api-key"];
            assert_eq!(key, "test-key");
            assert!(key.is_sensitive());
        }
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "c2ln");
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["id"],
            "call-a"
        );
    }

    #[test]
    fn unlisted_gemini_model_keeps_bound_spec_facts() {
        let vertex = ProviderId::catalog("vertexai").unwrap();
        let unlisted = Arc::new(ModelSpec::new(vertex, "gemini-4-neon").with_tools(true));
        let model = model_with_http(
            &Endpoint::default(),
            Some("test-key"),
            &unlisted,
            Catalog::builtin(),
            SequencedHttpClient::new([]),
        )
        .unwrap();
        let bound = model
            .spec()
            .expect("unlisted Agent Platform model must still expose bound facts");
        assert_eq!(bound.id, "gemini-4-neon");
        assert_eq!(bound.provider.vendor(), ADAPTER);
        assert!(bound.tools);
    }
}
