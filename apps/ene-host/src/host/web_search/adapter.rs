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
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;
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
) -> Result<reqwest::Response, ToolExecutionError> {
    let mut url = resolved.binding.endpoint.clone();
    let key = resolved.secret.as_deref().unwrap_or_default();
    let request = match resolved.adapter {
        SearchAdapter::Mwmbl => {
            if url.host_str() == Some("api.mwmbl.org") {
                take_mwmbl_slot(&MWMBL_NEXT_REQUEST).await;
            }
            url.query_pairs_mut().append_pair("q", query);
            client.get(url)
        }
        SearchAdapter::Searxng => {
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("format", "json");
            client.get(url)
        }
        SearchAdapter::Brave => {
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", &limit.to_string());
            client
                .get(url)
                .header("X-Subscription-Token", credential_header(key)?)
        }
        SearchAdapter::Tavily => client.post(url).bearer_auth(key).json(&json!({
            "query": query,
            "max_results": limit,
        })),
        SearchAdapter::Exa => client
            .post(url)
            .header("x-api-key", credential_header(key)?)
            .json(&json!({
                "query": query,
                "numResults": limit,
                "type": "auto",
                "contents": {"highlights": {"maxCharacters": 500}},
            })),
    };
    request
        .header(reqwest::header::USER_AGENT, SEARCH_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            ToolExecutionError::other(format!(
                "could not reach {}: {}",
                resolved.adapter.provider().name,
                error.without_url()
            ))
        })
}

fn credential_header(key: &str) -> Result<reqwest::header::HeaderValue, ToolExecutionError> {
    let mut value = reqwest::header::HeaderValue::from_str(key).map_err(|_| {
        ToolExecutionError::other("the search credential is not a valid header value")
    })?;
    value.set_sensitive(true);
    Ok(value)
}

// The anonymous public tier permits one request per second. The same queue
// admits searches and explicit probes. Cancellation while waiting drops the
// guard without reserving a future slot.
static MWMBL_NEXT_REQUEST: Mutex<Option<Instant>> = Mutex::const_new(None);

async fn take_mwmbl_slot(slot: &Mutex<Option<Instant>>) {
    let mut next = slot.lock().await;
    if let Some(deadline) = *next {
        tokio::time::sleep_until(deadline).await;
    }
    *next = Some(Instant::now() + Duration::from_secs(1));
}

pub(super) async fn read_http_error(
    response: reqwest::Response,
    resolved: &ResolvedSearch,
    status: reqwest::StatusCode,
) -> ToolExecutionError {
    let provider = resolved.adapter.provider().name;
    let detail = net::read_response_body(response, MAX_ERROR_BODY_BYTES, provider)
        .await
        .ok()
        .and_then(|bytes| extract_error_detail(&bytes, resolved.secret.as_deref()));
    let status = status.as_u16();
    let message = match detail {
        Some(detail) => format!("{provider} returned HTTP {status}: {detail}"),
        None => format!("{provider} returned HTTP {status}"),
    };
    ToolExecutionError::other(message)
}

fn extract_error_detail(body: &[u8], secret: Option<&str>) -> Option<String> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let detail = parsed
        .pointer("/detail/error")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("detail").and_then(Value::as_str))
        .or_else(|| parsed.get("error").and_then(Value::as_str))
        .or_else(|| parsed.get("message").and_then(Value::as_str))?;
    let trimmed = detail.trim();
    (!trimmed.is_empty() && !contains_credential(trimmed, secret)).then(|| truncate(trimmed, 200))
}

pub(super) fn contains_credential(text: &str, secret: Option<&str>) -> bool {
    secret.is_some_and(|secret| {
        text.contains(secret)
            || (text.contains('%')
                && url::form_urlencoded::parse(text.as_bytes())
                    .any(|(name, value)| name.contains(secret) || value.contains(secret)))
    })
}

pub(super) fn parse_adapter_results(
    adapter: SearchAdapter,
    provider_label: &str,
    body: &[u8],
    limit: usize,
    secret: Option<&str>,
) -> Result<Value, ToolExecutionError> {
    let results = match adapter {
        SearchAdapter::Mwmbl | SearchAdapter::Searxng | SearchAdapter::Tavily => {
            let parsed: MwmblOrSearxngResponse = decode(body, adapter)?;
            bounded_results(parsed.results, limit, secret)
        }
        SearchAdapter::Brave => {
            let parsed: BraveResponse = decode(body, adapter)?;
            let hits = parsed
                .web
                .into_iter()
                .flat_map(|web| web.results)
                .map(|hit| GenericHit {
                    url: hit.url,
                    title: hit.title,
                    content: hit.description,
                });
            bounded_results(hits, limit, secret)
        }
        SearchAdapter::Exa => {
            let parsed: ExaResponse = decode(body, adapter)?;
            let hits = parsed.results.into_iter().map(|hit| GenericHit {
                content: hit.best_snippet(),
                url: hit.url,
                title: hit.title,
            });
            bounded_results(hits, limit, secret)
        }
    };
    Ok(json!({"provider": provider_label, "results": results}))
}

fn decode<T: DeserializeOwned>(
    body: &[u8],
    adapter: SearchAdapter,
) -> Result<T, ToolExecutionError> {
    serde_json::from_slice(body).map_err(|_| {
        ToolExecutionError::other(format!(
            "{} returned invalid search results",
            adapter.provider().name
        ))
    })
}

fn bounded_results(
    hits: impl IntoIterator<Item = GenericHit>,
    limit: usize,
    secret: Option<&str>,
) -> Vec<SearchResult> {
    hits.into_iter()
        .filter_map(
            |GenericHit {
                 url: raw_url,
                 title,
                 content,
             }| {
                let url = url::Url::parse(&raw_url).ok()?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.as_str().len() > 2048
                    || [
                        raw_url.as_str(),
                        title.as_deref().unwrap_or(""),
                        content.as_deref().unwrap_or(""),
                    ]
                    .into_iter()
                    .any(|text| contains_credential(text, secret))
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
            },
        )
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn mwmbl_admission_spaces_concurrent_requests() {
        let slot = Mutex::new(None);
        let start = Instant::now();
        let take = || async {
            take_mwmbl_slot(&slot).await;
            start.elapsed()
        };
        let (one, two, three) = tokio::join!(biased; take(), take(), take());
        assert_eq!(
            [one, two, three],
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(2)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_does_not_take_a_slot() {
        let slot = Mutex::new(None);
        let start = Instant::now();
        take_mwmbl_slot(&slot).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), take_mwmbl_slot(&slot))
                .await
                .is_err()
        );
        take_mwmbl_slot(&slot).await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
}
