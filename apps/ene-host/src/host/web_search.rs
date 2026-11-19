//! Public web search for the built-in work agent.
use super::{background, db::truncate, net, resources::HostRuntime};
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
const ENDPOINT: &str = "https://api.mwmbl.org/api/v2/search/";
const MAX_QUERY_CHARS: usize = 500;
const MAX_RESULTS: usize = 10;
const DEFAULT_RESULTS: usize = 5;
const MAX_BODY_BYTES: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

struct WebSearch {
    runtime: Option<Handle>,
    next_request: Arc<Mutex<Option<Instant>>>,
}

pub(crate) fn register(app: &mut App) {
    let runtime = app
        .world()
        .get_resource::<HostRuntime>()
        .map(|runtime| runtime.0.clone());
    app.add_tool(WebSearch {
        runtime,
        next_request: Arc::default(),
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct Response {
    results: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    url: String,
    title: Option<String>,
    content: Option<String>,
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
        "Search the public Web using Mwmbl and return titles, source URLs and snippets. \
         Use public search terms and cite the source URLs when reporting findings."
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
        let limit = args.limit.unwrap_or(DEFAULT_RESULTS);
        if !(1..=MAX_RESULTS).contains(&limit) {
            return Err(ToolExecutionError::new(
                ToolErrorKind::InvalidArgs,
                format!("limit must be 1–{MAX_RESULTS}"),
            ));
        }
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| ToolExecutionError::other("the host HTTP runtime is unavailable"))?;
        let query = query.to_owned();
        let next_request = Arc::clone(&self.next_request);
        background::spawn(runtime, None, async move {
            tokio::time::timeout(TIMEOUT, async {
                {
                    let mut next = next_request.lock().await;
                    if let Some(deadline) = *next {
                        tokio::time::sleep_until(deadline).await;
                    }
                    // Mwmbl's anonymous tier allows one request per second.
                    *next = Some(Instant::now() + Duration::from_secs(1));
                }
                let mut url = url::Url::parse(ENDPOINT).expect("the Mwmbl endpoint is a valid URL");
                url.query_pairs_mut().append_pair("q", &query);
                let response = net::client()
                    .map_err(ToolExecutionError::other)?
                    .get(url)
                    .header(
                        reqwest::header::USER_AGENT,
                        concat!("ene/", env!("CARGO_PKG_VERSION")),
                    )
                    .send()
                    .await
                    .map_err(|error| {
                        ToolExecutionError::other(format!(
                            "could not reach Mwmbl: {}",
                            error.without_url()
                        ))
                    })?;
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(ToolExecutionError::other(
                        "Mwmbl anonymous search quota or rate limit reached",
                    ));
                }
                if !response.status().is_success() {
                    return Err(ToolExecutionError::other(format!(
                        "Mwmbl returned HTTP {}",
                        response.status().as_u16()
                    )));
                }
                let body = net::read_response_body(response, MAX_BODY_BYTES, "Mwmbl")
                    .await
                    .map_err(ToolExecutionError::other)?;
                parse_results(&body, limit)
            })
            .await
            .map_err(|_| ToolExecutionError::new(ToolErrorKind::Timeout, "Web search timed out"))?
        })
        .await
        .map_err(|_| ToolExecutionError::other("the web search task stopped"))?
    }
}

fn parse_results(body: &[u8], limit: usize) -> Result<Value, ToolExecutionError> {
    let response: Response = serde_json::from_slice(body)
        .map_err(|_| ToolExecutionError::other("Mwmbl returned invalid search results"))?;
    let results: Vec<_> = response
        .results
        .into_iter()
        .filter_map(|hit| {
            let url = url::Url::parse(&hit.url).ok()?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.as_str().len() > 2048
            {
                return None;
            }
            let title = hit
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty());
            Some(SearchResult {
                title: truncate(title.unwrap_or(url.as_str()), 300),
                url: url.into(),
                snippet: truncate(hit.content.as_deref().unwrap_or("").trim(), 500),
            })
        })
        .take(limit)
        .collect();
    Ok(json!({"provider": "Mwmbl", "results": results}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_keeps_citable_results_and_reports_invalid_responses() {
        let body = br#"{"results":[
            {"title":"unsafe","url":"javascript:alert(1)","content":"ignored"},
            {"title":"Rust","url":"https://www.rust-lang.org/","content":"Official site"},
            {"title":"Other","url":"https://example.org/","content":"beyond the limit"}
        ]}"#;
        let output = parse_results(body, 1).unwrap();
        assert_eq!(
            output["results"],
            json!([{"title":"Rust","url":"https://www.rust-lang.org/","snippet":"Official site"}])
        );
        assert_eq!(
            parse_results(br#"{"results":[]}"#, 5).unwrap()["results"],
            json!([])
        );
        assert!(parse_results(b"service unavailable", 5).is_err());
    }
}
