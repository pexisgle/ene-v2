use super::super::{
    error::{HostError, Result},
    json, net,
    settings::Endpoint,
};
use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, Uri};
use rig_core::{
    catalog::{ModelFacts, ModelSpec},
    completion::CompletionRequest,
    driver::{DynModel, Model},
    error::ProviderError,
    http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse},
    operation::Completion,
    providers::gemini::{GeminiConfig, completion::GenerateContent},
};
use std::sync::Arc;

pub(crate) const ADAPTER: &str = "google-agent-platform";
pub(crate) const ENDPOINT: &str = "https://aiplatform.googleapis.com";
pub(super) const DEFAULT_MODEL: &str = "gemini-3.8-flash";
const PROJECT_MODELS: &[&str] = &[
    DEFAULT_MODEL,
    "gemini-4-argon",
    "gemini-3.7-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3.1-pro-preview",
];
const EXPRESS_MODELS: &[&str] = &[
    DEFAULT_MODEL,
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
];

fn valid_resource(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn valid_model(id: &str) -> bool {
    id.starts_with("gemini-")
        && id.len() > "gemini-".len()
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        })
}

fn publisher_endpoint(endpoint: &Endpoint) -> Result<String> {
    let base = endpoint.endpoint.trim().trim_end_matches('/');
    if !base.is_empty() && base != ENDPOINT {
        return Err(HostError::Invalid(
            "Agent Platform derives the endpoint from the Google Cloud location".into(),
        ));
    }
    let options = json::parse_object(&endpoint.options)?;
    let field = |name: &str| -> Result<&str> {
        match options.get(name) {
            None => Ok(""),
            Some(value) => value.as_str().map(str::trim).ok_or_else(|| {
                HostError::Invalid(format!("Agent Platform {name} must be a string"))
            }),
        }
    };
    let project = field("project_id")?;
    let location = field("location")?;
    if project.is_empty() {
        if !location.is_empty() {
            return Err(HostError::Invalid(
                "Agent Platform requires a project ID when a location is set".into(),
            ));
        }
        return Ok(format!("{ENDPOINT}/v1/publishers/google/models"));
    }
    let location = if location.is_empty() {
        "global"
    } else {
        location
    };
    if !valid_resource(project) || !valid_resource(location) {
        return Err(HostError::Invalid(
            "Agent Platform project ID and location must use lowercase letters, digits or hyphens"
                .into(),
        ));
    }
    let host = match location {
        "global" => ENDPOINT.into(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        _ => format!("https://{location}-aiplatform.googleapis.com"),
    };
    Ok(format!(
        "{host}/v1/projects/{project}/locations/{location}/publishers/google/models"
    ))
}

fn express(publisher: &str) -> bool {
    publisher == format!("{ENDPOINT}/v1/publishers/google/models")
}

pub(super) fn fallback_models(endpoint: &Endpoint) -> &'static [&'static str] {
    if publisher_endpoint(endpoint).is_ok_and(|publisher| express(&publisher)) {
        EXPRESS_MODELS
    } else {
        PROJECT_MODELS
    }
}

fn model_list_url(publisher: &str) -> String {
    let (host, _) = publisher
        .split_once("/v1/")
        .expect("validated publisher URL");
    format!("{host}/v1beta1/publishers/google/models")
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

pub(super) async fn list_models(
    endpoint: &Endpoint,
    secret: Option<&str>,
) -> std::result::Result<Vec<String>, String> {
    let publisher = publisher_endpoint(endpoint).map_err(|error| error.to_string())?;
    let key = api_key(secret).map_err(|error| error.to_string())?;
    tokio::time::timeout(
        super::PROBE_TIMEOUT,
        list_publisher_models(&net::client()?, &model_list_url(&publisher), key),
    )
    .await
    .map_err(|_| "Google Agent Platform のモデル一覧の取得がタイムアウトしました".to_owned())?
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPage {
    #[serde(default)]
    publisher_models: Vec<PublisherModel>,
    #[serde(default)]
    next_page_token: String,
}

#[derive(serde::Deserialize)]
struct PublisherModel {
    name: String,
}

fn chat_model_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix("publishers/google/models/")?;
    (valid_model(id)
        && !id.split('-').any(|part| {
            matches!(
                part,
                "embedding" | "image" | "live" | "tts" | "omni" | "cyber" | "banana"
            )
        }))
    .then_some(id)
}

async fn list_publisher_models(
    client: &reqwest::Client,
    url: &str,
    key: HeaderValue,
) -> std::result::Result<Vec<String>, String> {
    let mut models = Vec::new();
    let mut token = String::new();
    let base = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    for _ in 0..32 {
        let mut page_url = base.clone();
        page_url.query_pairs_mut().append_pair("pageSize", "100");
        if !token.is_empty() {
            page_url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let response = client
            .get(page_url)
            .header("x-goog-api-key", key.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| error.without_url().to_string())?;
        let body = net::read_response_body(
            response,
            2 * 1024 * 1024,
            "Google Agent Platform model list",
        )
        .await?;
        let page: ModelPage = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        for model in page.publisher_models {
            if let Some(id) = chat_model_id(&model.name)
                && !models.iter().any(|known| known == id)
            {
                models.push(id.to_owned());
            }
        }
        if page.next_page_token.is_empty() {
            return Ok(models);
        }
        if page.next_page_token == token {
            break;
        }
        token = page.next_page_token;
    }
    Err("Google Agent Platform model list did not finish".into())
}

pub(crate) fn validate(endpoint: &Endpoint, model_ids: &[String]) -> Result<()> {
    publisher_endpoint(endpoint)?;
    if model_ids.iter().any(|id| !valid_model(id.trim())) {
        return Err(HostError::Invalid(
            "Agent Platform model IDs must be Gemini model names such as gemini-3.8-flash".into(),
        ));
    }
    Ok(())
}

pub(super) fn model(
    endpoint: &Endpoint,
    secret: Option<String>,
    spec: Arc<ModelSpec>,
) -> Result<DynModel<Completion>> {
    let client = net::model_client().map_err(HostError::Unavailable)?;
    model_with_http(
        endpoint,
        secret,
        spec,
        rig_reqwest::ReqwestClient::from(client),
    )
}

fn model_with_http(
    endpoint: &Endpoint,
    secret: Option<String>,
    spec: Arc<ModelSpec>,
    http: impl HttpClientExt + Clone + 'static,
) -> Result<DynModel<Completion>> {
    let publisher = publisher_endpoint(endpoint)?;
    if !valid_model(&spec.id) {
        return Err(HostError::Invalid(
            "Invalid Agent Platform Gemini model ID".into(),
        ));
    }
    let key = api_key(secret.as_deref())?;
    let wire = GenerateContent::new(GeminiConfig::new("").with_base_url(ENDPOINT), &spec.id)
        .with_facts(ModelFacts::new(spec.clone()));
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

pub(super) async fn probe(model: DynModel<Completion>) -> std::result::Result<(), String> {
    let request = CompletionRequest::new("Reply with OK.").max_tokens(64);
    tokio::time::timeout(super::PROBE_TIMEOUT, model.call(request))
        .await
        .map_err(|_| "Google Agent Platform の接続確認がタイムアウトしました".to_owned())?
        .map(|_| ())
        .map_err(probe_error)
}

fn probe_error(error: ProviderError) -> String {
    let status = match &error {
        ProviderError::InvalidAuthentication(response)
        | ProviderError::ProviderResponse(response) => response.status,
        ProviderError::Http(error) => error.non_success_status(),
        _ => None,
    };
    match status.map(|status| status.as_u16()) {
        Some(400 | 401 | 403) => {
            "Google Agent Platform がリクエストを拒否しました。API キー、プロジェクトと権限を確認してください".into()
        }
        Some(404) => "Google Agent Platform で指定したモデルまたはロケーションを利用できません".into(),
        Some(status) => format!("Google Agent Platform の接続確認に失敗しました（HTTP {status}）"),
        None => "Google Agent Platform から生成結果を取得できませんでした".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::{
        catalog::Catalog,
        completion::ToolDefinition,
        message::{Message, ToolName, ToolResultContent, UserContent},
        test_utils::{MockHttpResponse, RecordingHttpClient, SequencedHttpClient},
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
            let host = expected.split_once("/v1/").unwrap().0;
            assert_eq!(model_list_url(&expected), format!("{host}/v1beta1/publishers/google/models"));
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
    async fn lists_all_pages_and_keeps_only_gemini_chat_models() {
        let app = axum::Router::new().route(
            "/v1beta1/publishers/google/models",
            axum::routing::get(|uri: Uri, headers: http::HeaderMap| async move {
                assert_eq!(headers["x-goog-api-key"], "test-key");
                let model = |id: &str| json!({"name": format!("publishers/google/models/{id}")});
                axum::Json(match uri.query() {
                    Some("pageSize=100") => json!({
                        "publisherModels": [model("gemini-3.8-flash"), model("gemini-3.8-live"),
                            model("gemini-3-pro-image"), model("gemini-embedding-2")],
                        "nextPageToken": "next+/="
                    }),
                    Some("pageSize=100&pageToken=next%2B%2F%3D") => json!({
                        "publisherModels": [model("gemini-3.8-flash"), model("gemini-4-argon"),
                            model("gemma-4"), model("gemini-3.8-flash-tts")]
                    }),
                    _ => panic!("unexpected model list query: {uri}"),
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1beta1/publishers/google/models",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let models = list_publisher_models(
            &net::client().unwrap(),
            &url,
            api_key(Some("test-key")).unwrap(),
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(models, ["gemini-3.8-flash", "gemini-4-argon"]);
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
            Some("test-key".into()),
            spec(),
            http.clone(),
        )
        .unwrap();
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

    #[tokio::test]
    async fn checks_the_configured_project_and_reports_rejected_credentials() {
        let endpoint = Endpoint {
            options: r#"{"project_id":"my-project","location":"global"}"#.into(),
            ..Default::default()
        };
        let http = RecordingHttpClient::with_error(http::StatusCode::FORBIDDEN, "test-key");
        assert!(model_with_http(&endpoint, None, spec(), http.clone()).is_err());
        assert!(http.requests().is_empty());
        let model =
            model_with_http(&endpoint, Some("test-key".into()), spec(), http.clone()).unwrap();
        let error = probe(model).await.unwrap_err();
        assert!(error.contains("API キー"));
        assert!(!error.contains("test-key"));
        assert_eq!(
            http.requests()[0].uri,
            format!(
                "{ENDPOINT}/v1/projects/my-project/locations/global/publishers/google/models/{DEFAULT_MODEL}:generateContent"
            )
        );
    }
}
