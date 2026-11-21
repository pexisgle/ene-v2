//! Public web search for the built-in work agent driven strictly by saved
//! `Search` connections (`exa`, `tavily`, `brave`, `mwmbl`, `searxng`).
use super::{
    background,
    db::Store,
    error::Failure,
    model::{Credentials, connection_secret},
    net,
    providers::Providers,
    resources::HostRuntime,
    settings::{Check, Connection, ConnectionConfig, ConnectionKind, endpoint_of},
    util::{self, truncate},
};
use rig_core::{
    completion::ToolAnnotations,
    tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError},
};
use rig_ecs::prelude::{App, AppToolsExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::Mutex, time::Instant};

pub(crate) const NAME: &str = "web_search";
const MWMBL_DEFAULT_ENDPOINT: &str = "https://api.mwmbl.org/api/v2/search/";
const BRAVE_DEFAULT_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_DEFAULT_ENDPOINT: &str = "https://api.tavily.com/search";
const EXA_DEFAULT_ENDPOINT: &str = "https://api.exa.ai/search";
const MAX_QUERY_CHARS: usize = 500;
const MAX_RESULTS: usize = 20;
const DEFAULT_RESULTS: usize = 5;
const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
const TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchAdapter {
    Mwmbl,
    Searxng,
    Brave,
    Tavily,
    Exa,
}

impl SearchAdapter {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mwmbl" => Some(Self::Mwmbl),
            "searxng" => Some(Self::Searxng),
            "brave" => Some(Self::Brave),
            "tavily" => Some(Self::Tavily),
            "exa" => Some(Self::Exa),
            _ => None,
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Mwmbl => "Mwmbl",
            Self::Searxng => "SearXNG",
            Self::Brave => "Brave",
            Self::Tavily => "Tavily",
            Self::Exa => "Exa",
        }
    }

    fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Mwmbl => Some(MWMBL_DEFAULT_ENDPOINT),
            Self::Searxng => None,
            Self::Brave => Some(BRAVE_DEFAULT_ENDPOINT),
            Self::Tavily => Some(TAVILY_DEFAULT_ENDPOINT),
            Self::Exa => Some(EXA_DEFAULT_ENDPOINT),
        }
    }

    fn requires_api_key(self) -> bool {
        matches!(self, Self::Brave | Self::Tavily | Self::Exa)
    }
}

fn normalize_endpoint(adapter: SearchAdapter, raw: String) -> String {
    if adapter != SearchAdapter::Mwmbl {
        return raw;
    }
    // `endpoint_of` strips trailing slashes, while Mwmbl's Django endpoint
    // redirects `/api/v2/search` -> `/api/v2/search/` with HTTP 301 and the
    // host HTTP client intentionally keeps redirects off.
    if let Ok(mut url) = url::Url::parse(&raw) {
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        return url.into();
    }
    raw
}

#[derive(Clone)]
pub(crate) struct ResolvedSearch {
    pub(crate) adapter: SearchAdapter,
    pub(crate) provider_name: String,
    pub(crate) endpoint: String,
    pub(crate) max_results: Option<usize>,
    secret: Option<String>,
}

/// Resolves the active enabled `Search` connection from saved settings.
/// Refuses cleanly when no enabled search connection exists — no hardcoded
/// endpoint fallback runs behind the user's back.
pub(crate) fn resolve(
    store: &Store,
    credentials: Option<&Credentials>,
    providers: &Providers,
) -> Result<ResolvedSearch, String> {
    let settings = store
        .settings()
        .map_err(|error| format!("could not read search settings: {error}"))?;
    let enabled: Vec<&Connection> = settings
        .connections
        .iter()
        .filter(|conn| conn.enabled && conn.kind == ConnectionKind::Search)
        .collect();
    if enabled.is_empty() {
        return Err(
            "search_unconfigured: Web検索を使うには設定画面で有効な Search 接続（Exa / Tavily / Brave / Mwmbl / SearXNG）を登録してください"
                .into(),
        );
    }
    let mut first_error = None;
    for connection in enabled {
        match resolve_connection(connection, credentials, providers) {
            Ok(resolved) => return Ok(resolved),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    Err(first_error
        .unwrap_or_else(|| "search_unconfigured: 有効な Web 検索接続を構成できませんでした".into()))
}

pub(crate) fn resolve_connection(
    connection: &Connection,
    credentials: Option<&Credentials>,
    providers: &Providers,
) -> Result<ResolvedSearch, String> {
    let ConnectionConfig::Search(cfg) = &connection.config else {
        return Err("search_unconfigured: invalid Search connection configuration".into());
    };
    let adapter = SearchAdapter::parse(&cfg.endpoint.adapter).ok_or_else(|| {
        format!(
            "search_unconfigured: unsupported search adapter `{}`",
            cfg.endpoint.adapter
        )
    })?;
    let endpoint = endpoint_of(connection, providers)
        .or_else(|| adapter.default_endpoint().map(str::to_owned))
        .map(|raw| normalize_endpoint(adapter, raw))
        .ok_or_else(|| {
            format!(
                "search_unconfigured: `{}` connection requires an endpoint URL",
                adapter.display_name()
            )
        })?;
    let secret = connection_secret(connection, credentials, providers)
        .map_err(|error| error.to_string())?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if adapter.requires_api_key() && secret.is_none() {
        return Err(format!(
            "search_unconfigured: `{}` search connection requires an API key credential",
            adapter.display_name()
        ));
    }
    let provider_name = if connection.name.trim().is_empty() {
        adapter.display_name().to_owned()
    } else {
        connection.name.clone()
    };
    let max_results =
        (cfg.max_results > 0).then_some((cfg.max_results as usize).clamp(1, MAX_RESULTS));
    Ok(ResolvedSearch {
        adapter,
        provider_name,
        endpoint,
        max_results,
        secret,
    })
}

/// Probes a `Search` connection from Settings -> CheckConnection.
pub(crate) async fn probe(
    connection: &Connection,
    credentials: &Credentials,
    providers: &Providers,
) -> Check {
    let checked_at = util::now();
    let resolved = match resolve_connection(connection, Some(credentials), providers) {
        Ok(resolved) => resolved,
        Err(message) => {
            return Check {
                healthy: false,
                checked_at,
                failure: Some(Failure::new("connection_check", message)),
                capabilities: Vec::new(),
                resources: Vec::new(),
            };
        }
    };
    let throttle = Arc::new(Mutex::new(None));
    match execute(&resolved, "rust programming", 1, &throttle).await {
        Ok(_) => Check {
            healthy: true,
            checked_at,
            failure: None,
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
        Err(error) => Check {
            healthy: false,
            checked_at,
            failure: Some(Failure::new("connection_check", error.to_string())),
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
    }
}

struct WebSearch {
    runtime: Option<Handle>,
    wake: Option<rig_ecs::tasks::Wake>,
    store: Option<Store>,
    credentials: Option<Credentials>,
    providers: Providers,
    mwmbl_next_request: Arc<Mutex<Option<Instant>>>,
}

pub(crate) fn register(app: &mut App) {
    let world = app.world();
    let runtime = world
        .get_resource::<HostRuntime>()
        .map(|r| r.0.clone())
        .or_else(|| tokio::runtime::Handle::try_current().ok());
    let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
    let store = world.get_resource::<Store>().cloned();
    let credentials = world.get_resource::<Credentials>().cloned();
    let providers = world
        .get_resource::<Providers>()
        .cloned()
        .unwrap_or_default();
    app.add_tool(WebSearch {
        runtime,
        wake,
        store,
        credentials,
        providers,
        mwmbl_next_request: Arc::default(),
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct MwmblOrSearxngResponse {
    results: Vec<GenericHit>,
}

#[derive(Deserialize)]
struct GenericHit {
    url: String,
    title: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveHit>,
}

#[derive(Deserialize)]
struct BraveHit {
    url: String,
    title: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ExaResponse {
    results: Vec<ExaHit>,
}

#[derive(Deserialize)]
struct ExaHit {
    url: String,
    title: Option<String>,
    highlights: Option<Vec<String>>,
    summary: Option<String>,
    snippet: Option<String>,
    text: Option<String>,
}

impl ExaHit {
    fn best_snippet(&self) -> Option<String> {
        if let Some(highlights) = &self.highlights {
            let joined: Vec<&str> = highlights
                .iter()
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !joined.is_empty() {
                return Some(joined.join(" ... "));
            }
        }
        [
            self.summary.as_deref(),
            self.snippet.as_deref(),
            self.text.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
    }
}

#[derive(Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

impl Tool for WebSearch {
    const NAME: &'static str = NAME;
    type Args = SearchArgs;
    type Output = Value;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Search the public Web using the configured search connection and return titles, \
         source URLs and snippets. Cite the source URLs when reporting findings."
            .into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_CHARS},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "default": DEFAULT_RESULTS},
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: SearchArgs,
    ) -> Result<Value, ToolExecutionError> {
        let query = args.query.trim();
        if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
            return Err(ToolExecutionError::new(
                ToolErrorKind::InvalidArgs,
                format!("query must be 1–{MAX_QUERY_CHARS} characters"),
            ));
        }
        let requested_limit = args.limit.unwrap_or(DEFAULT_RESULTS);
        if !(1..=MAX_RESULTS).contains(&requested_limit) {
            return Err(ToolExecutionError::new(
                ToolErrorKind::InvalidArgs,
                format!("limit must be 1–{MAX_RESULTS}"),
            ));
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| ToolExecutionError::other("host settings store is unavailable"))?;
        let resolved = resolve(store, self.credentials.as_ref(), &self.providers)
            .map_err(ToolExecutionError::other)?;
        let limit = resolved
            .max_results
            .map_or(requested_limit, |cap| requested_limit.min(cap));
        let current_runtime = tokio::runtime::Handle::try_current().ok();
        let runtime = self
            .runtime
            .as_ref()
            .or(current_runtime.as_ref())
            .ok_or_else(|| ToolExecutionError::other("the host HTTP runtime is unavailable"))?;
        let query = query.to_owned();
        let throttle = Arc::clone(&self.mwmbl_next_request);
        background::spawn(runtime, self.wake.as_ref(), async move {
            execute(&resolved, &query, limit, &throttle).await
        })
        .await
        .map_err(|_| ToolExecutionError::other("the web search task stopped"))?
    }
}

// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_lines)]
async fn execute(
    resolved: &ResolvedSearch,
    query: &str,
    limit: usize,
    mwmbl_next_request: &Arc<Mutex<Option<Instant>>>,
) -> Result<Value, ToolExecutionError> {
    tokio::time::timeout(TIMEOUT, async {
        let client = net::client().map_err(ToolExecutionError::other)?;
        let ua = concat!("ene/", env!("CARGO_PKG_VERSION"));
        let response = match resolved.adapter {
            SearchAdapter::Mwmbl => {
                let mut url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid search endpoint URL"))?;
                if !url.path().ends_with('/') {
                    let path = format!("{}/", url.path());
                    url.set_path(&path);
                }
                if url
                    .host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case("api.mwmbl.org"))
                {
                    let mut next = mwmbl_next_request.lock().await;
                    if let Some(deadline) = *next {
                        tokio::time::sleep_until(deadline).await;
                    }
                    // Mwmbl's anonymous tier allows one request per second.
                    *next = Some(Instant::now() + Duration::from_secs(1));
                }
                url.query_pairs_mut().append_pair("q", query);
                client
                    .get(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await
            }
            SearchAdapter::Searxng => {
                let mut url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid SearXNG endpoint URL"))?;
                if !url.path().ends_with("/search")
                    && let Ok(mut segments) = url.path_segments_mut()
                {
                    segments.pop_if_empty().push("search");
                }
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("format", "json");
                client
                    .get(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await
            }
            SearchAdapter::Brave => {
                let mut url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid Brave Search URL"))?;
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &limit.to_string());
                let key = resolved.secret.as_deref().unwrap_or_default().trim();
                client
                    .get(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header("X-Subscription-Token", key)
                    .send()
                    .await
            }
            SearchAdapter::Tavily => {
                let url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid Tavily Search URL"))?;
                let key = resolved.secret.as_deref().unwrap_or_default().trim();
                client
                    .post(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .bearer_auth(key)
                    .json(&json!({
                        "api_key": key,
                        "query": query,
                        "max_results": limit,
                    }))
                    .send()
                    .await
            }
            SearchAdapter::Exa => {
                let url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid Exa Search URL"))?;
                let key = resolved.secret.as_deref().unwrap_or_default().trim();
                client
                    .post(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header("x-api-key", key)
                    .json(&json!({
                        "query": query,
                        "numResults": limit,
                        "type": "auto",
                        "contents": {
                            "highlights": {
                                "maxCharacters": 500,
                            }
                        }
                    }))
                    .send()
                    .await
            }
        }
        .map_err(|error| {
            ToolExecutionError::other(format!(
                "could not reach {}: {}",
                resolved.adapter.display_name(),
                error.without_url()
            ))
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ToolExecutionError::other(format!(
                "{} quota or rate limit reached",
                resolved.adapter.display_name()
            )));
        }
        if !status.is_success() {
            let detail = net::read_response_body(
                response,
                MAX_ERROR_BODY_BYTES,
                resolved.adapter.display_name(),
            )
            .await
            .ok()
            .and_then(|bytes| extract_error_detail(&bytes));
            let message = match detail {
                Some(detail) => format!(
                    "{} returned HTTP {}: {detail}",
                    resolved.adapter.display_name(),
                    status.as_u16()
                ),
                None => format!(
                    "{} returned HTTP {}",
                    resolved.adapter.display_name(),
                    status.as_u16()
                ),
            };
            return Err(ToolExecutionError::other(message));
        }
        let body =
            net::read_response_body(response, MAX_BODY_BYTES, resolved.adapter.display_name())
                .await
                .map_err(ToolExecutionError::other)?;
        parse_adapter_results(resolved.adapter, &resolved.provider_name, &body, limit)
    })
    .await
    .map_err(|_| ToolExecutionError::new(ToolErrorKind::Timeout, "Web search timed out"))?
}

fn extract_error_detail(body: &[u8]) -> Option<String> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let detail = parsed
        .pointer("/detail/error")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("detail").and_then(Value::as_str))
        .or_else(|| parsed.get("error").and_then(Value::as_str))
        .or_else(|| parsed.get("message").and_then(Value::as_str))?;
    let trimmed = detail.trim();
    (!trimmed.is_empty()).then(|| truncate(trimmed, 200))
}

fn parse_adapter_results(
    adapter: SearchAdapter,
    provider_label: &str,
    body: &[u8],
    limit: usize,
) -> Result<Value, ToolExecutionError> {
    let raw_hits: Vec<(String, Option<String>, Option<String>)> = match adapter {
        SearchAdapter::Mwmbl | SearchAdapter::Searxng | SearchAdapter::Tavily => {
            let parsed: MwmblOrSearxngResponse = serde_json::from_slice(body).map_err(|_| {
                ToolExecutionError::other(format!(
                    "{} returned invalid search results",
                    adapter.display_name()
                ))
            })?;
            parsed
                .results
                .into_iter()
                .map(|hit| (hit.url, hit.title, hit.content))
                .collect()
        }
        SearchAdapter::Brave => {
            let parsed: BraveResponse = serde_json::from_slice(body)
                .map_err(|_| ToolExecutionError::other("Brave returned invalid search results"))?;
            parsed
                .web
                .map(|web| web.results)
                .unwrap_or_default()
                .into_iter()
                .map(|hit| (hit.url, hit.title, hit.description))
                .collect()
        }
        SearchAdapter::Exa => {
            let parsed: ExaResponse = serde_json::from_slice(body)
                .map_err(|_| ToolExecutionError::other("Exa returned invalid search results"))?;
            parsed
                .results
                .into_iter()
                .map(|hit| {
                    let snippet = hit.best_snippet();
                    (hit.url, hit.title, snippet)
                })
                .collect()
        }
    };
    let results: Vec<_> = raw_hits
        .into_iter()
        .filter_map(|(raw_url, title, content)| {
            let url = url::Url::parse(&raw_url).ok()?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.as_str().len() > 2048
            {
                return None;
            }
            let title = title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty());
            Some(SearchResult {
                title: truncate(title.unwrap_or(url.as_str()), 300),
                url: url.into(),
                snippet: truncate(content.as_deref().unwrap_or("").trim(), 500),
            })
        })
        .take(limit)
        .collect();
    Ok(json!({"provider": provider_label, "results": results}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{CredentialState, Endpoint, SearchConnection};

    #[test]
    fn search_keeps_citable_results_and_reports_invalid_responses() {
        let body = br#"{"results":[
            {"title":"unsafe","url":"javascript:alert(1)","content":"ignored"},
            {"title":"Rust","url":"https://www.rust-lang.org/","content":"Official site"},
            {"title":"Other","url":"https://example.org/","content":"beyond the limit"}
        ]}"#;
        let output = parse_adapter_results(SearchAdapter::Mwmbl, "Mwmbl", body, 1).unwrap();
        assert_eq!(
            output["results"],
            json!([{"title":"Rust","url":"https://www.rust-lang.org/","snippet":"Official site"}])
        );
        assert_eq!(
            parse_adapter_results(SearchAdapter::Mwmbl, "Mwmbl", br#"{"results":[]}"#, 5).unwrap()
                ["results"],
            json!([])
        );
        assert!(
            parse_adapter_results(SearchAdapter::Mwmbl, "Mwmbl", b"service unavailable", 5)
                .is_err()
        );
    }

    #[tokio::test]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    async fn f13_configured_search_adapters_execute_and_respect_credentials() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };

        let app = Router::new()
            .route(
                "/mwmbl",
                get(|| async { http::StatusCode::MOVED_PERMANENTLY }),
            )
            .route(
                "/mwmbl/",
                get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(q.get("q").map(String::as_str), Some("rust"));
                    Json(json!({"results":[{"url":"https://mwmbl.example/1","title":"Mwmbl Hit","content":"mwmbl ok"}]}))
                }),
            )
            .route(
                "/searxng/search",
                get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(q.get("format").map(String::as_str), Some("json"));
                    Json(json!({"results":[{"url":"https://searx.example/1","title":"SearX Hit","content":"ok"}]}))
                }),
            )
            .route(
                "/brave",
                get(|headers: http::HeaderMap| async move {
                    assert_eq!(headers["x-subscription-token"], "brave-secret");
                    Json(json!({"web":{"results":[{"url":"https://brave.example/1","title":"Brave Hit","description":"brave desc"}]}}))
                }),
            )
            .route(
                "/tavily",
                post(|headers: http::HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers[http::header::AUTHORIZATION], "Bearer tavily-secret");
                    assert_eq!(body["api_key"], "tavily-secret");
                    assert_eq!(body["query"], "rust");
                    Json(json!({"results":[{"url":"https://tavily.example/1","title":"Tavily Hit","content":"tavily desc"}]}))
                }),
            )
            .route(
                "/exa",
                post(|headers: http::HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["x-api-key"], "exa-secret");
                    assert_eq!(body["query"], "rust");
                    assert_eq!(body["numResults"], 5);
                    assert_eq!(body["type"], "auto");
                    assert_eq!(body["contents"]["highlights"]["maxCharacters"], 500);
                    Json(json!({
                        "results": [
                            {
                                "url": "https://exa.example/1",
                                "title": "Exa Hit",
                                "highlights": ["exa highlight 1", "exa highlight 2"]
                            },
                            {
                                "url": "https://exa.example/2",
                                "title": "Exa Fallback",
                                "text": "exa text fallback"
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/exa-unauthorized",
                post(|| async {
                    (
                        http::StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "Invalid API key"})),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let creds = Credentials::new(true);
        creds.remember("s-brave", "brave-secret".into());
        creds.remember("s-tavily", "  tavily-secret \n".into());
        creds.remember("s-exa", "exa-secret ".into());
        creds.remember("s-exa-bad", "wrong-key".into());
        let providers = Providers::none();
        let throttle = Arc::new(Mutex::new(None));

        // Mwmbl must keep its trailing slash even though endpoint_of trims it.
        let mwmbl_conn = Connection {
            id: "s-mwmbl".into(),
            name: "Mwmbl".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "mwmbl".into(),
                    endpoint: format!("http://{addr}/mwmbl/"),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        let resolved_mwmbl = resolve_connection(&mwmbl_conn, Some(&creds), &providers).unwrap();
        assert!(resolved_mwmbl.endpoint.ends_with('/'));
        let out = execute(&resolved_mwmbl, "rust", 5, &throttle)
            .await
            .unwrap();
        assert_eq!(out["results"][0]["title"], "Mwmbl Hit");

        let searx_conn = Connection {
            id: "s-searx".into(),
            name: "Local SearXNG".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "searxng".into(),
                    endpoint: format!("http://{addr}/searxng"),
                    ..Default::default()
                },
                max_results: 3,
            }),
        };
        let resolved_searx = resolve_connection(&searx_conn, Some(&creds), &providers).unwrap();
        let out = execute(&resolved_searx, "rust", 5, &throttle)
            .await
            .unwrap();
        assert_eq!(out["results"][0]["title"], "SearX Hit");

        let brave_conn = Connection {
            id: "s-brave".into(),
            name: "Brave".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Registered,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "brave".into(),
                    endpoint: format!("http://{addr}/brave"),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        let resolved_brave = resolve_connection(&brave_conn, Some(&creds), &providers).unwrap();
        let out = execute(&resolved_brave, "rust", 5, &throttle)
            .await
            .unwrap();
        assert_eq!(out["results"][0]["snippet"], "brave desc");

        let tavily_conn = Connection {
            id: "s-tavily".into(),
            name: "Tavily".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Registered,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "tavily".into(),
                    endpoint: format!("http://{addr}/tavily"),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        let resolved_tavily = resolve_connection(&tavily_conn, Some(&creds), &providers).unwrap();
        let out = execute(&resolved_tavily, "rust", 5, &throttle)
            .await
            .unwrap();
        assert_eq!(out["results"][0]["title"], "Tavily Hit");

        let exa_conn = Connection {
            id: "s-exa".into(),
            name: "Exa".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Registered,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "exa".into(),
                    endpoint: format!("http://{addr}/exa"),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        assert_eq!(
            exa_conn.environment_variable(&providers).as_deref(),
            Some("EXA_API_KEY")
        );
        assert_eq!(
            tavily_conn.environment_variable(&providers).as_deref(),
            Some("TAVILY_API_KEY")
        );
        let resolved_exa = resolve_connection(&exa_conn, Some(&creds), &providers).unwrap();
        let out = execute(&resolved_exa, "rust", 5, &throttle).await.unwrap();
        assert_eq!(out["results"][0]["title"], "Exa Hit");
        assert_eq!(
            out["results"][0]["snippet"],
            "exa highlight 1 ... exa highlight 2"
        );
        assert_eq!(out["results"][1]["snippet"], "exa text fallback");

        // Missing Exa credential refuses cleanly.
        let exa_no_key = Connection {
            id: "s-exa-missing".into(),
            ..exa_conn.clone()
        };
        let missing_err = resolve_connection(&exa_no_key, Some(&creds), &providers)
            .err()
            .expect("missing Exa credential must fail");
        assert!(missing_err.contains("requires an API key credential"));

        // Non-2xx error body detail is surfaced cleanly.
        let exa_bad = Connection {
            id: "s-exa-bad".into(),
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "exa".into(),
                    endpoint: format!("http://{addr}/exa-unauthorized"),
                    ..Default::default()
                },
                max_results: 5,
            }),
            ..exa_conn
        };
        let resolved_bad = resolve_connection(&exa_bad, Some(&creds), &providers).unwrap();
        let err = execute(&resolved_bad, "rust", 5, &throttle)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 401"));
        assert!(err.to_string().contains("Invalid API key"));

        server.abort();
    }

    #[test]
    fn resolve_skips_unconfigured_enabled_search_when_another_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
        let creds = Credentials::new(true);
        creds.remember("search-exa", "exa-valid-key".into());
        let providers = Providers::none();

        let rev = store.settings_revision().unwrap();
        // `a-unconfigured-tavily` sorts before `search-exa` by id, but has no key.
        let unconfigured = Connection {
            id: "a-unconfigured-tavily".into(),
            name: "Tavily (no key)".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "tavily".into(),
                    endpoint: TAVILY_DEFAULT_ENDPOINT.into(),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        let ready_exa = Connection {
            id: "search-exa".into(),
            name: "Web検索 (Exa)".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Registered,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "exa".into(),
                    endpoint: EXA_DEFAULT_ENDPOINT.into(),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        store
            .update_settings(rev, &[(unconfigured, 0), (ready_exa, 0)])
            .unwrap();

        let resolved = resolve(&store, Some(&creds), &providers).unwrap();
        assert_eq!(resolved.adapter, SearchAdapter::Exa);
        assert_eq!(resolved.endpoint, EXA_DEFAULT_ENDPOINT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    async fn f13_b04_unconfigured_search_refuses_without_http_and_gates_on_configured_host() {
        use crate::host::agent::WorkAgent;
        use crate::host::tests::Harness;
        use axum::{Json, Router, routing::get};
        use bevy_ecs::prelude::With;
        use rig_core::completion::Message;
        use rig_core::message::{ToolResultContent, UserContent};
        use rig_core::test_utils::MockStreamEvent;
        use rig_ecs::prelude::Conversation;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let req_hits = Arc::clone(&hits);
        let app = Router::new().route(
            "/searx/search",
            get(move || {
                req_hits.fetch_add(1, Ordering::SeqCst);
                async {
                    Json(json!({"results":[{"url":"https://ok.example/","title":"Configured Hit","content":"verified"}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Part 1: no Search connection configured -> Gate refuses before approval or HTTP.
        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call("call-1", NAME, json!({"query": "rust"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("検索接続が未設定です"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("了解"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("call-2", NAME, json!({"query": "rust"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("見つかりました"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("完了"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let companion_id = harness.companion.clone();
        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id: companion_id.clone(),
                instruction: "検索して".into(),
                ..Default::default()
            },
        )
        .unwrap();
        harness.run_until(|world| {
            let work_refused = world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| match &entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == NAME
                                && result.is_error
                                && result.content.iter().any(|block|
                                    matches!(block, ToolResultContent::Text(text)
                                        if text.text.contains("search_unconfigured"))))
                        }),
                        _ => false,
                    })
                });
            let companion_settled = world
                .query_filtered::<&Conversation, With<crate::host::agent::CompanionAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| {
                        matches!(&entry.message, Message::Assistant(reply)
                            if reply.text().contains("了解"))
                    })
                });
            work_refused && companion_settled
        });
        assert!(
            harness
                .store()
                .approvals(crate::host::permission::ApprovalStatus::Pending)
                .unwrap()
                .is_empty(),
            "unconfigured web_search must fail before creating an approval card"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        // Part 2: configuring Search connection + Allow policy for "web_search" executes against configured URL.
        let rev = harness.store().settings_revision().unwrap();
        let search_conn = Connection {
            id: "search-1".into(),
            name: "Local SearXNG".into(),
            enabled: true,
            kind: ConnectionKind::Search,
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Search(SearchConnection {
                endpoint: Endpoint {
                    adapter: "searxng".into(),
                    endpoint: format!("http://{addr}/searx"),
                    ..Default::default()
                },
                max_results: 5,
            }),
        };
        harness
            .store()
            .update_settings(rev, &[(search_conn, 0)])
            .unwrap();
        harness
            .store()
            .apply_policy_rules(
                &[crate::host::permission::PolicyRule {
                    id: "allow-search".into(),
                    tool_name: NAME.into(),
                    pattern: "*".into(),
                    effect: crate::host::permission::PolicyEffect::Allow,
                    created_at: util::now(),
                }],
                &[],
            )
            .unwrap();
        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id,
                instruction: "再検索して".into(),
                ..Default::default()
            },
        )
        .unwrap();
        harness.run_until(|world| {
            world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| match &entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == NAME
                                && !result.is_error
                                && result.content.iter().any(|block| match block {
                                    ToolResultContent::Text(text) => {
                                        text.text.contains("Configured Hit")
                                    }
                                    ToolResultContent::Json { value } => {
                                        value.to_string().contains("Configured Hit")
                                    }
                                    _ => false,
                                }))
                        }),
                        _ => false,
                    })
                })
        });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    async fn exa_and_tavily_work_end_to_end_and_switch_cleanly_in_work_agent() {
        use crate::host::agent::WorkAgent;
        use crate::host::tests::Harness;
        use axum::{Json, Router, routing::post};
        use bevy_ecs::prelude::With;
        use rig_core::completion::Message;
        use rig_core::message::{ToolResultContent, UserContent};
        use rig_core::test_utils::MockStreamEvent;
        use rig_ecs::prelude::Conversation;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let exa_hits = Arc::new(AtomicUsize::new(0));
        let tavily_hits = Arc::new(AtomicUsize::new(0));
        let exa_counter = Arc::clone(&exa_hits);
        let tavily_counter = Arc::clone(&tavily_hits);

        let app = Router::new()
            .route(
                "/exa",
                post(move |headers: http::HeaderMap, Json(body): Json<Value>| {
                    let exa_counter = Arc::clone(&exa_counter);
                    async move {
                        assert_eq!(headers["x-api-key"], "exa-live-key");
                        assert!(body["query"].as_str().is_some_and(|q| !q.is_empty()));
                        exa_counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "results": [{
                                "url": "https://exa.ai/docs",
                                "title": "Exa Live Hit",
                                "highlights": ["Exa semantic snippet"]
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/tavily",
                post(move |headers: http::HeaderMap, Json(body): Json<Value>| {
                    let tavily_counter = Arc::clone(&tavily_counter);
                    async move {
                        assert_eq!(
                            headers[http::header::AUTHORIZATION],
                            "Bearer tavily-live-key"
                        );
                        assert_eq!(body["api_key"], "tavily-live-key");
                        assert!(body["query"].as_str().is_some_and(|q| !q.is_empty()));
                        tavily_counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "results": [{
                                "url": "https://tavily.com/docs",
                                "title": "Tavily Live Hit",
                                "content": "Tavily snippet"
                            }]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call("call-exa", NAME, json!({"query": "rust exa"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("Exaで見つかりました"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("Exa完了"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("call-tavily", NAME, json!({"query": "rust tavily"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("Tavilyで見つかりました"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("Tavily完了"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let companion_id = harness.companion.clone();

        // Register keys in the harness credential store and allow web_search.
        {
            let creds = harness.app.world().resource::<Credentials>().clone();
            creds.remember("search-exa", "exa-live-key".into());
            creds.remember("search-tavily", "tavily-live-key".into());
        }
        harness
            .store()
            .apply_policy_rules(
                &[crate::host::permission::PolicyRule {
                    id: "allow-web-search".into(),
                    tool_name: NAME.into(),
                    pattern: "*".into(),
                    effect: crate::host::permission::PolicyEffect::Allow,
                    created_at: util::now(),
                }],
                &[],
            )
            .unwrap();

        let make_conn =
            |id: &str, name: &str, adapter: &str, path: &str, enabled: bool| Connection {
                id: id.into(),
                name: name.into(),
                enabled,
                kind: ConnectionKind::Search,
                credential_state: CredentialState::Registered,
                revision: 0,
                last_check: None,
                config: ConnectionConfig::Search(SearchConnection {
                    endpoint: Endpoint {
                        adapter: adapter.into(),
                        endpoint: format!("http://{addr}{path}"),
                        ..Default::default()
                    },
                    max_results: 5,
                }),
            };

        // 1. Activate Exa and verify probe + WorkAgent web_search.
        let exa_conn = make_conn("search-exa", "Web検索 (Exa)", "exa", "/exa", true);
        let tavily_conn = make_conn(
            "search-tavily",
            "Web検索 (Tavily)",
            "tavily",
            "/tavily",
            false,
        );
        let rev = harness.store().settings_revision().unwrap();
        harness
            .store()
            .update_settings(rev, &[(exa_conn.clone(), 0), (tavily_conn.clone(), 0)])
            .unwrap();
        let probe_creds = harness.app.world().resource::<Credentials>().clone();
        assert!(
            probe(&exa_conn, &probe_creds, &Providers::none())
                .await
                .healthy
        );
        assert!(
            probe(&tavily_conn, &probe_creds, &Providers::none())
                .await
                .healthy
        );

        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id: companion_id.clone(),
                instruction: "Exaで検索して".into(),
                ..Default::default()
            },
        )
        .unwrap();
        harness.run_until(|world| {
            let exa_ok = world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| match &entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == NAME
                                && !result.is_error
                                && result.content.iter().any(|block| match block {
                                    ToolResultContent::Text(text) => {
                                        text.text.contains("Exa Live Hit")
                                    }
                                    ToolResultContent::Json { value } => {
                                        value.to_string().contains("Exa Live Hit")
                                    }
                                    _ => false,
                                }))
                        }),
                        _ => false,
                    })
                });
            let companion_settled = world
                .query_filtered::<&Conversation, With<crate::host::agent::CompanionAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| {
                        matches!(&entry.message, Message::Assistant(reply)
                            if reply.text().contains("Exa完了"))
                    })
                });
            exa_ok && companion_settled
        });

        // 2. Switch active search to Tavily and verify WorkAgent uses Tavily.
        let settings = harness.store().settings().unwrap();
        let exa_off = Connection {
            enabled: false,
            ..exa_conn
        };
        let tavily_on = Connection {
            enabled: true,
            ..tavily_conn
        };
        harness
            .store()
            .update_settings(settings.revision, &[(exa_off, 1), (tavily_on, 1)])
            .unwrap();
        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id,
                instruction: "Tavilyで検索して".into(),
                ..Default::default()
            },
        )
        .unwrap();
        harness.run_until(|world| {
            world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| match &entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == NAME
                                && !result.is_error
                                && result.content.iter().any(|block| match block {
                                    ToolResultContent::Text(text) => {
                                        text.text.contains("Tavily Live Hit")
                                    }
                                    ToolResultContent::Json { value } => {
                                        value.to_string().contains("Tavily Live Hit")
                                    }
                                    _ => false,
                                }))
                        }),
                        _ => false,
                    })
                })
        });

        // 1 probe + 1 WorkAgent search each = 2 hits per provider.
        assert_eq!(exa_hits.load(Ordering::SeqCst), 2);
        assert_eq!(tavily_hits.load(Ordering::SeqCst), 2);
        server.abort();
    }
}
