//! The adapter wire layer: one request shape and one response shape per
//! enabled search service, plus the shared hit filter that turns any of them
//! into the citable result list.
//!
//! Every function here is the same for all adapters except where the service
//! dictates otherwise, so the differences sit in one `match` on
//! [`SearchAdapter`] instead of a trait with five implementors.
use super::{ResolvedSearch, SearchAdapter};
use crate::host::net;
use ene_store::util::truncate;
use rig_core::tool::ToolExecutionError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
const SEARCH_USER_AGENT: &str = concat!("ene/", env!("CARGO_PKG_VERSION"));

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

pub(super) async fn send_search_request(
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
        // Reserve the slot under the lock, then wait for it outside: holding
        // the guard across the sleep would queue every later request behind
        // this one's wait instead of letting them run up to the same deadline.
        let deadline = {
            let mut next = mwmbl_next_request.lock().await;
            let deadline = *next;
            // Mwmbl's anonymous tier allows one request per second.
            *next = Some(Instant::now() + Duration::from_secs(1));
            deadline
        };
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(deadline).await;
        }
    }
    url.query_pairs_mut().append_pair("q", query);
    Ok(client
        .get(url)
        .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await)
}

pub(super) async fn read_http_error(
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

pub(super) fn parse_adapter_results(
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
                .map_or_default(|web| web.results)
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
