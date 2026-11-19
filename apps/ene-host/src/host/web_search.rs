//! Public web search for the built-in work agent driven strictly by saved
//! `Search` connections (`mwmbl`, `searxng`, `brave`, `tavily`).
use super::{
    background,
    db::{self, Store, truncate},
    error::Failure,
    model::{Credentials, connection_secret},
    net,
    providers::Providers,
    resources::HostRuntime,
    settings::{Check, Connection, ConnectionConfig, ConnectionKind, endpoint_of},
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
const MAX_QUERY_CHARS: usize = 500;
const MAX_RESULTS: usize = 20;
const DEFAULT_RESULTS: usize = 5;
const MAX_BODY_BYTES: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchAdapter {
    Mwmbl,
    Searxng,
    Brave,
    Tavily,
}

impl SearchAdapter {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mwmbl" => Some(Self::Mwmbl),
            "searxng" => Some(Self::Searxng),
            "brave" => Some(Self::Brave),
            "tavily" => Some(Self::Tavily),
            _ => None,
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Mwmbl => "Mwmbl",
            Self::Searxng => "SearXNG",
            Self::Brave => "Brave",
            Self::Tavily => "Tavily",
        }
    }

    fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Mwmbl => Some(MWMBL_DEFAULT_ENDPOINT),
            Self::Searxng => None,
            Self::Brave => Some(BRAVE_DEFAULT_ENDPOINT),
            Self::Tavily => Some(TAVILY_DEFAULT_ENDPOINT),
        }
    }
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
    let Some(connection) = settings
        .connections
        .iter()
        .find(|conn| conn.enabled && conn.kind == ConnectionKind::Search)
    else {
        return Err(
            "search_unconfigured: Web検索を使うには設定画面で有効な Search 接続（Mwmbl / SearXNG / Brave / Tavily）を登録してください"
                .into(),
        );
    };
    resolve_connection(connection, credentials, providers)
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
        .ok_or_else(|| {
            format!(
                "search_unconfigured: `{}` connection requires an endpoint URL",
                adapter.display_name()
            )
        })?;
    let secret = connection_secret(connection, credentials, providers)
        .map_err(|error| error.to_string())?
        .filter(|value| !value.trim().is_empty());
    if matches!(adapter, SearchAdapter::Brave | SearchAdapter::Tavily) && secret.is_none() {
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
    let checked_at = db::now();
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
    match execute(&resolved, "ene", 1, &throttle).await {
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
                if resolved.endpoint == MWMBL_DEFAULT_ENDPOINT {
                    let mut next = mwmbl_next_request.lock().await;
                    if let Some(deadline) = *next {
                        tokio::time::sleep_until(deadline).await;
                    }
                    // Mwmbl's anonymous tier allows one request per second.
                    *next = Some(Instant::now() + Duration::from_secs(1));
                }
                let mut url = url::Url::parse(&resolved.endpoint)
                    .map_err(|_| ToolExecutionError::other("invalid search endpoint URL"))?;
                url.query_pairs_mut().append_pair("q", query);
                client
                    .get(url)
                    .header(reqwest::header::USER_AGENT, ua)
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
                let key = resolved.secret.as_deref().unwrap_or_default();
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
                let key = resolved.secret.as_deref().unwrap_or_default();
                client
                    .post(url)
                    .header(reqwest::header::USER_AGENT, ua)
                    .json(&json!({
                        "api_key": key,
                        "query": query,
                        "max_results": limit,
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
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ToolExecutionError::other(format!(
                "{} quota or rate limit reached",
                resolved.adapter.display_name()
            )));
        }
        if !response.status().is_success() {
            return Err(ToolExecutionError::other(format!(
                "{} returned HTTP {}",
                resolved.adapter.display_name(),
                response.status().as_u16()
            )));
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
    async fn f13_configured_search_adapters_execute_and_respect_credentials() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };

        let app = Router::new()
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
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["api_key"], "tavily-secret");
                    Json(json!({"results":[{"url":"https://tavily.example/1","title":"Tavily Hit","content":"tavily desc"}]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let creds = Credentials::new(true);
        creds.remember("s-brave", "brave-secret".into());
        creds.remember("s-tavily", "tavily-secret".into());
        let providers = Providers::none();
        let throttle = Arc::new(Mutex::new(None));

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

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
                    created_at: db::now(),
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
}
