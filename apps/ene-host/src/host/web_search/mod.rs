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
use strum::{EnumString, IntoStaticStr};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(ascii_case_insensitive)]
pub(crate) enum SearchAdapter {
    /// The parse key is the lowercase configuration spelling; the label is what
    /// the user-facing error text and the search result header show.
    #[strum(serialize = "mwmbl", to_string = "Mwmbl")]
    Mwmbl,
    #[strum(serialize = "searxng", to_string = "SearXNG")]
    Searxng,
    #[strum(serialize = "brave", to_string = "Brave")]
    Brave,
    #[strum(serialize = "tavily", to_string = "Tavily")]
    Tavily,
    #[strum(serialize = "exa", to_string = "Exa")]
    Exa,
}

impl SearchAdapter {
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }

    fn display_name(self) -> &'static str {
        self.into()
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

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SearchConfigError(pub(crate) String);

impl From<&'static str> for SearchConfigError {
    fn from(message: &'static str) -> Self {
        Self(message.to_owned())
    }
}

/// Resolves the active enabled `Search` connection from saved settings.
/// Refuses cleanly when no enabled search connection exists — no hardcoded
/// endpoint fallback runs behind the user's back.
pub(crate) fn resolve(
    store: &Store,
    credentials: Option<&Credentials>,
    providers: &Providers,
) -> Result<ResolvedSearch, SearchConfigError> {
    let settings = store
        .settings_repo()
        .settings()
        .map_err(|error| SearchConfigError(format!("could not read search settings: {error}")))?;
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
) -> Result<ResolvedSearch, SearchConfigError> {
    let ConnectionConfig::Search(cfg) = &connection.config else {
        return Err("search_unconfigured: invalid Search connection configuration".into());
    };
    let adapter = SearchAdapter::parse(&cfg.endpoint.adapter).ok_or_else(|| {
        SearchConfigError(format!(
            "search_unconfigured: unsupported search adapter `{}`",
            cfg.endpoint.adapter
        ))
    })?;
    let endpoint = endpoint_of(connection, providers)
        .or_else(|| adapter.default_endpoint().map(str::to_owned))
        .map(|raw| normalize_endpoint(adapter, raw))
        .ok_or_else(|| {
            SearchConfigError(format!(
                "search_unconfigured: `{}` connection requires an endpoint URL",
                adapter.display_name()
            ))
        })?;
    let secret = connection_secret(connection, credentials, providers)
        .map_err(|error| SearchConfigError(error.to_string()))?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if adapter.requires_api_key() && secret.is_none() {
        return Err(SearchConfigError(format!(
            "search_unconfigured: `{}` search connection requires an API key credential",
            adapter.display_name()
        )));
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
        Err(error) => {
            return Check {
                healthy: false,
                checked_at,
                failure: Some(Failure::new("connection_check", error.to_string())),
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct SearchArgs {
    #[schemars(length(min = 1, max = MAX_QUERY_CHARS))]
    query: String,
    // `default` is a no-op on `Option`, and it is what keeps `limit` out of
    // `required` now that `with` supplies the plain integer shape.
    #[serde(default)]
    #[schemars(
        with = "usize",
        range(min = 1, max = MAX_RESULTS),
        extend("default" = DEFAULT_RESULTS),
        transform = plain_integer
    )]
    limit: Option<usize>,
}

/// schemars names the integer width in `format`; the model-facing contract
/// carries no format keyword.
fn plain_integer(schema: &mut schemars::Schema) {
    schema.remove("format");
}

impl WebSearch {
    /// The tool-calling schema, derived from `SearchArgs` so it cannot drift
    /// from the type the arguments decode into.
    fn schema_for_arguments() -> Value {
        let schema = schemars::schema_for!(SearchArgs);
        let mut schema = serde_json::to_value(schema).unwrap_or(Value::Null);
        // The tool contract carries no root `$schema` or `title`.
        if let Some(object) = schema.as_object_mut() {
            object.remove("$schema");
            object.remove("title");
        }
        schema
    }
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
        Self::schema_for_arguments()
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
            .map_err(|error| ToolExecutionError::other(error.to_string()))?;
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

const SEARCH_USER_AGENT: &str = concat!("ene/", env!("CARGO_PKG_VERSION"));

async fn execute(
    resolved: &ResolvedSearch,
    query: &str,
    limit: usize,
    mwmbl_next_request: &Arc<Mutex<Option<Instant>>>,
) -> Result<Value, ToolExecutionError> {
    tokio::time::timeout(TIMEOUT, async {
        let client = net::client().map_err(|error| ToolExecutionError::other(error.to_string()))?;
        let response =
            send_search_request(&client, resolved, query, limit, mwmbl_next_request).await?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ToolExecutionError::other(format!(
                "{} quota or rate limit reached",
                resolved.adapter.display_name()
            )));
        }
        if !status.is_success() {
            return Err(read_http_error(response, resolved.adapter, status).await);
        }
        let body =
            net::read_response_body(response, MAX_BODY_BYTES, resolved.adapter.display_name())
                .await
                .map_err(|error| ToolExecutionError::other(error.to_string()))?;
        parse_adapter_results(resolved.adapter, &resolved.provider_name, &body, limit)
    })
    .await
    .map_err(|_| ToolExecutionError::new(ToolErrorKind::Timeout, "Web search timed out"))?
}

async fn send_search_request(
    client: &reqwest::Client,
    resolved: &ResolvedSearch,
    query: &str,
    limit: usize,
    mwmbl_next_request: &Arc<Mutex<Option<Instant>>>,
) -> Result<reqwest::Response, ToolExecutionError> {
    let sent = match resolved.adapter {
        SearchAdapter::Mwmbl => {
            send_mwmbl_request(client, &resolved.endpoint, query, mwmbl_next_request).await?
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
                .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
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
                .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
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
                .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
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
                .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
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
    };
    sent.map_err(|error| {
        ToolExecutionError::other(format!(
            "could not reach {}: {}",
            resolved.adapter.display_name(),
            error.without_url()
        ))
    })
}

async fn send_mwmbl_request(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    mwmbl_next_request: &Arc<Mutex<Option<Instant>>>,
) -> Result<Result<reqwest::Response, reqwest::Error>, ToolExecutionError> {
    let mut url = url::Url::parse(endpoint)
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
    Ok(client
        .get(url)
        .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await)
}

async fn read_http_error(
    response: reqwest::Response,
    adapter: SearchAdapter,
    status: reqwest::StatusCode,
) -> ToolExecutionError {
    let detail = net::read_response_body(response, MAX_ERROR_BODY_BYTES, adapter.display_name())
        .await
        .ok()
        .and_then(|bytes| extract_error_detail(&bytes));
    let message = match detail {
        Some(detail) => format!(
            "{} returned HTTP {}: {detail}",
            adapter.display_name(),
            status.as_u16()
        ),
        None => format!(
            "{} returned HTTP {}",
            adapter.display_name(),
            status.as_u16()
        ),
    };
    ToolExecutionError::other(message)
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
mod tests;
