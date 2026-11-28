use crate::error::{invalid, missing};
use anyhow::{Context as _, Result};
use bytes::Bytes;
use ene_domain::agent_platform::{self, ENDPOINT, valid_model};
use ene_rpc::Connection;
use http::{HeaderValue, Method, Request, Response, Uri};
use rig_core::{
    catalog::Catalog,
    http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse},
    operation::Completion,
    providers::{
        gemini::{GeminiConfig, completion::GenerateContent},
        registry::ProviderId,
    },
};
use serde::Deserialize;
use std::{future::Future, time::Duration};

const CANDIDATES: &[&str] = &[
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-flash",
    "gemini-2.5-pro",
];

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    project_id: String,
    location: String,
}

pub(crate) fn publisher(connection: &Connection) -> Result<String> {
    let options: Options = if connection.options_json.trim().is_empty() {
        Options::default()
    } else {
        serde_json::from_str(&connection.options_json)
            .map_err(|_| invalid("invalid Google Agent Platform options"))?
    };
    agent_platform::publisher(
        &connection.endpoint,
        options.project_id.trim(),
        options.location.trim(),
    )
    .map_err(|_| invalid("invalid Google Agent Platform publisher"))
}

#[derive(Clone)]
pub(super) struct AgentPlatform {
    http: reqwest::Client,
    publisher: String,
    key: HeaderValue,
}

impl AgentPlatform {
    pub(super) fn new(
        http: reqwest::Client,
        connection: &Connection,
        secret: Option<&str>,
    ) -> Result<Self> {
        let publisher = publisher(connection)?;
        let mut key = HeaderValue::from_str(secret.filter(|key| !key.is_empty()).ok_or({
            crate::error::ApiError(
                connectrpc::ErrorCode::Unauthenticated,
                "Google API key missing",
            )
        })?)
        .map_err(|_| invalid("invalid Google API key"))?;
        key.set_sensitive(true);
        Ok(Self {
            http,
            publisher,
            key,
        })
    }

    pub(super) fn completion(&self, model: &str) -> Result<rig_core::DynModel<Completion>> {
        if !valid_model(model) {
            return Err(invalid("invalid Google Agent Platform model ID"));
        }
        let provider =
            ProviderId::catalog("gcp.gemini").context("Gemini is absent from Rig's catalog")?;
        let wire = GenerateContent::new(GeminiConfig::new("").with_base_url(ENDPOINT), model)
            .with_facts(Catalog::builtin().facts(provider, model));
        Ok(rig_core::Model::new(
            wire,
            PublisherHttp {
                http: rig_reqwest::ReqwestClient::from(self.http.clone()),
                publisher: self.publisher.clone(),
                key: self.key.clone(),
            },
        )
        .erase())
    }

    pub(super) async fn models(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for batch in CANDIDATES.chunks(4) {
            let requests: Vec<_> = batch
                .iter()
                .map(|&model| self.verify_model(model))
                .collect();
            for (model, available) in futures_util::future::join_all(requests).await {
                if available {
                    names.push(model.to_owned());
                }
            }
        }
        if names.is_empty() {
            return Err(missing("no Google Agent Platform models could be verified"));
        }
        Ok(names)
    }
    async fn verify_model(&self, model: &'static str) -> (&'static str, bool) {
        let available = self
            .http
            .post(format!("{}/{model}:countTokens", self.publisher))
            .header("x-goog-api-key", self.key.clone())
            .json(&serde_json::json!({"contents":[{"role":"user","parts":[{"text":"Hi"}]}]}))
            .timeout(Duration::from_secs(4))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        (model, available)
    }
}

#[derive(Clone)]
struct PublisherHttp {
    http: rig_reqwest::ReqwestClient,
    publisher: String,
    key: HeaderValue,
}

impl PublisherHttp {
    fn route<T>(&self, mut request: Request<T>) -> Option<Request<T>> {
        if request.method() != Method::POST {
            return None;
        }
        let (model, verb) = request
            .uri()
            .path()
            .strip_prefix("/v1beta/models/")?
            .split_once(':')?;
        if !valid_model(model) {
            return None;
        }
        let suffix = match verb {
            "generateContent" => verb,
            "streamGenerateContent" => "streamGenerateContent?alt=sse",
            _ => return None,
        };
        *request.uri_mut() = format!("{}/{model}:{suffix}", self.publisher)
            .parse::<Uri>()
            .ok()?;
        request
            .headers_mut()
            .insert("x-goog-api-key", self.key.clone());
        Some(request)
    }
}

fn unsupported() -> http_client::Error {
    http_client::Error::permanent(std::io::Error::other(
        "unsupported Google Agent Platform operation",
    ))
}

impl HttpClientExt for PublisherHttp {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        let sent = self.route(request).map(|request| self.http.send(request));
        async move { sent.ok_or_else(unsupported)?.await }
    }

    fn send_multipart(
        &self,
        _: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        std::future::ready(Err(unsupported()))
    }

    async fn send_streaming(
        &self,
        request: Request<Bytes>,
    ) -> http_client::Result<StreamingResponse> {
        self.http
            .send_streaming(self.route(request).ok_or_else(unsupported)?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_routes_only_generate_content_and_sets_a_sensitive_key() -> Result<()> {
        let publisher = PublisherHttp {
            http: rig_reqwest::ReqwestClient::from(reqwest::Client::new()),
            publisher: agent_platform::publisher(ENDPOINT, "test-project", "asia-northeast1")?,
            key: {
                let mut key = HeaderValue::from_static("fixture");
                key.set_sensitive(true);
                key
            },
        };
        let request =
            Request::post("/v1beta/models/gemini-test:streamGenerateContent?alt=sse").body(())?;
        let routed = publisher.route(request).expect("Gemini generation route");
        assert_eq!(
            routed.uri().to_string(),
            "https://asia-northeast1-aiplatform.googleapis.com/v1/projects/test-project/locations/asia-northeast1/publishers/google/models/gemini-test:streamGenerateContent?alt=sse",
        );
        assert!(routed.headers()["x-goog-api-key"].is_sensitive());
        let other = Request::post("/v1beta/models/gemini-test:countTokens").body(())?;
        assert!(publisher.route(other).is_none());
        Ok(())
    }
}
