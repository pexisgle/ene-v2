use crate::approval::ApprovalCommands as _;

use crate::{
    actor::Host,
    approval::Operation,
    error::{conflict, denied, invalid},
    model::truncate,
    store,
};
use anyhow::{Result, ensure};
use ene_core::policy::{Action, Effect, Target};
use ene_rpc::Connection;
use rig_core::{completion::ToolDefinition, message::ToolName};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::{sync::Mutex, time::Instant};

static MWMBL_NEXT: Mutex<Option<Instant>> = Mutex::const_new(None);

async fn wait_for_slot(adapter: Adapter, url: &url::Url) {
    if matches!(adapter, Adapter::Mwmbl) && url.host_str() == Some("api.mwmbl.org") {
        let mut next = MWMBL_NEXT.lock().await;
        if let Some(deadline) = *next {
            tokio::time::sleep_until(deadline).await;
        }
        *next = Some(Instant::now() + Duration::from_secs(1));
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Adapter {
    Mwmbl,
    Searxng,
    Brave,
    Tavily,
    Exa,
}

impl Adapter {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "mwmbl" => Ok(Self::Mwmbl),
            "searxng" => Ok(Self::Searxng),
            "brave" => Ok(Self::Brave),
            "tavily" => Ok(Self::Tavily),
            "exa" => Ok(Self::Exa),
            _ => Err(invalid("unknown web search adapter")),
        }
    }

    fn requires_key(self) -> bool {
        matches!(self, Self::Brave | Self::Tavily | Self::Exa)
    }
}

pub(crate) fn definition() -> Result<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("web_search")?,
        "Search the public web using the configured provider. Results include source URLs.",
        json!({"type":"object","properties":{
            "query":{"type":"string","minLength":1,"maxLength":500},
            "limit":{"type":"integer","minimum":1,"maximum":20}
        },"required":["query"],"additionalProperties":false}),
    ))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    limit: Option<u8>,
}

pub(crate) async fn invoke(
    host: &Host,
    operation: &Operation,
    connection: &Connection,
    args: Map<String, Value>,
) -> Result<String> {
    let mut search: Search = serde_json::from_value(Value::Object(args))?;
    search.query = search.query.trim().to_owned();
    ensure!(
        (1..=500).contains(&search.query.chars().count()),
        "invalid search query"
    );
    let limit = search.limit.unwrap_or(5).clamp(1, 20);
    let adapter = Adapter::parse(&connection.adapter)?;
    let endpoint = crate::model::check_endpoint(&connection.endpoint)?;
    let action = Action::new(
        Target {
            receiver: format!("web-search/{}", endpoint.origin().ascii_serialization()),
            revision: store::fingerprint(&(
                connection.id.as_str(),
                connection.revision,
                endpoint.as_str(),
            ))?,
            operation: "web_search".into(),
            effect: Effect::Send,
            device: None,
        },
        Map::from_iter([
            ("query".into(), json!(&search.query)),
            ("limit".into(), json!(limit)),
        ]),
    )?;
    let digest = host.authorize(operation, action.clone()).await?;
    let models = host
        .call(|actor| Box::pin(async { Ok(actor.models.clone()) }))
        .await?;
    let key = if adapter.requires_key() {
        let key = models.credential(connection).await?;
        if key.as_deref().is_none_or(str::is_empty) {
            return Err(denied("search credential missing"));
        }
        key
    } else {
        None
    };
    tokio::select! {
        biased;
        () = operation.cancel.cancelled() => return Err(denied("work stopped")),
        () = wait_for_slot(adapter, &endpoint) => {}
    }
    let admitted = connection.clone();
    let scope = operation.clone();
    host.call(move |actor| {
        Box::pin(async move {
            actor.check_operation(&scope.companion, &scope.run, &action)?;
            action.verify(&digest)?;
            if actor.connection(&admitted.id).await? != admitted || scope.cancel.is_cancelled() {
                return Err(conflict("search connection changed"));
            }
            Ok(())
        })
    })
    .await?;
    let bytes = tokio::select! {
        biased;
        () = operation.cancel.cancelled() => return Err(denied("work stopped")),
        result = fetch(&models.http, adapter, endpoint, &search.query, limit, key.as_deref()) => result?,
    };
    let body: Value = serde_json::from_slice(&bytes)?;
    Ok(parse_hits(adapter, &connection.name, &body, limit).to_string())
}

async fn fetch(
    client: &reqwest::Client,
    adapter: Adapter,
    mut url: url::Url,
    query: &str,
    limit: u8,
    key: Option<&str>,
) -> Result<Vec<u8>> {
    let key = key.unwrap_or_default();
    let request = match adapter {
        Adapter::Mwmbl => {
            if !url.path().ends_with('/') {
                url.path_segments_mut().map_err(|_| invalid("invalid search URL"))?.push("");
            }
            client.get(url).query(&[("q", query)])
        }
        Adapter::Searxng => {
            if !url.path().ends_with("/search") {
                url.path_segments_mut().map_err(|_| invalid("invalid search URL"))?.pop_if_empty().push("search");
            }
            client.get(url).query(&[("q", query), ("format", "json")])
        }
        Adapter::Brave => client.get(url)
            .header("X-Subscription-Token", key)
            .query(&[("q", query), ("count", &limit.to_string())]),
        Adapter::Tavily => client.post(url).bearer_auth(key)
            .json(&json!({"query": query, "max_results": limit})),
        Adapter::Exa => client.post(url).header("x-api-key", key)
            .json(&json!({"query":query, "numResults":limit, "type":"auto", "contents":{"highlights":{"maxCharacters":500}}})),
    };
    let mut response = request
        .header(
            reqwest::header::USER_AGENT,
            concat!("ene/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        ensure!(
            bytes.len() + chunk.len() <= 512 * 1024,
            "search response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_hits(adapter: Adapter, provider: &str, data: &Value, limit: u8) -> Value {
    let items = match adapter {
        Adapter::Brave => data.pointer("/web/results").and_then(Value::as_array),
        Adapter::Mwmbl => data.as_array().or_else(|| data["results"].as_array()),
        _ => data["results"].as_array(),
    };
    let hits: Vec<_> = items.into_iter().flatten().filter_map(|hit| {
        let url = url::Url::parse(hit["url"].as_str()?).ok()?;
        if !ene_core::policy::safe_url(&url) || url.as_str().len() > 2048 {
            return None;
        }
        let title = hit["title"].as_str().filter(|value| !value.trim().is_empty()).unwrap_or(url.as_str());
        let snippet = ["content", "description", "extract", "summary", "snippet", "text"].into_iter()
            .find_map(|key| hit[key].as_str())
            .or_else(|| hit["highlights"].as_array().and_then(|items| items.first()).and_then(Value::as_str))
            .unwrap_or_default();
        Some(json!({"title":truncate(title,300), "url":url.as_str(), "snippet":truncate(snippet,500)}))
    }).take(usize::from(limit)).collect();
    json!({"provider":provider,"results":hits})
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, body::to_bytes, extract::Request};

    #[tokio::test]
    async fn search_adapters_use_their_respective_request_and_response_formats() -> Result<()> {
        let router = axum::Router::new().fallback(|request: Request| async move {
            let (parts, body) = request.into_parts();
            let url = parts.uri;
            let query = url.query().unwrap_or_default();
            let hit =
                json!({"title":"Source","url":"https://example.com/","description":"An answer"});
            let key = |name| {
                parts
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
            };
            match url.path() {
                "/mwmbl/" => {
                    assert_eq!(query, "q=rust");
                    Json(json!([hit]))
                }
                "/searx/search" => {
                    assert_eq!(query, "q=rust&format=json");
                    Json(json!({"results":[hit]}))
                }
                "/brave" => {
                    assert_eq!(query, "q=rust&count=1");
                    assert_eq!(key("x-subscription-token"), Some("fixture"));
                    Json(json!({"web":{"results":[hit]}}))
                }
                path => {
                    let adapter = path.to_owned();
                    if adapter == "/tavily" {
                        assert_eq!(key("authorization"), Some("Bearer fixture"));
                    } else {
                        assert_eq!(&adapter, "/exa");
                        assert_eq!(key("x-api-key"), Some("fixture"));
                    }
                    let body = to_bytes(body, 1024).await.unwrap();
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["query"], "rust");
                    assert_eq!(
                        body[if adapter == "/tavily" {
                            "max_results"
                        } else {
                            "numResults"
                        }],
                        1
                    );
                    Json(json!({"results":[hit]}))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let fixture = tokio::spawn(async move { axum::serve(listener, router).await });
        let client = reqwest::Client::builder().no_proxy().build()?;
        for (adapter, path) in [
            (Adapter::Mwmbl, "mwmbl"),
            (Adapter::Searxng, "searx"),
            (Adapter::Brave, "brave"),
            (Adapter::Tavily, "tavily"),
            (Adapter::Exa, "exa"),
        ] {
            let bytes = fetch(
                &client,
                adapter,
                format!("{base}/{path}").parse()?,
                "rust",
                1,
                Some("fixture"),
            )
            .await?;
            let hits = parse_hits(adapter, path, &serde_json::from_slice(&bytes)?, 1);
            assert_eq!(hits["results"][0]["url"], "https://example.com/");
        }
        fixture.abort();
        Ok(())
    }
}
