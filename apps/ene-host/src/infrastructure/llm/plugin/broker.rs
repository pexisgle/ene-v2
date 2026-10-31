//! Only the host adds credentials. Redirects and unapproved origins are rejected.
use super::manifest::Bundle;
use crate::{
    domain::{Error, ErrorKind},
    llm::Invocation,
    ports::Result,
};
use base64::Engine;
use ene_plugin::{
    HttpRequest,
    provider::{AuthProfile, BrokerRouting},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn denied() -> Error {
    Error::new(ErrorKind::PermissionRequired)
}
pub async fn send(
    bundle: Bundle,
    input: std::sync::Arc<Invocation>,
    request: HttpRequest,
) -> Result<crate::llm::Response> {
    let models = input.operation() == crate::domain::OperationKind::ModelList;
    let HttpRequest {
        method,
        url,
        headers,
        body_base64,
    } = request;
    let body = body_base64
        .map(|body| base64::engine::general_purpose::STANDARD.decode(body))
        .transpose()
        .map_err(|_| Error::invalid())?;
    input.authorization().check().await?;
    let base = url::Url::parse(&input.connection().endpoint).map_err(|_| denied())?;
    let target = url::Url::parse(&url).map_err(|_| denied())?;
    if target.origin() != base.origin()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || !bundle
            .manifest
            .destinations
            .contains(&target.origin().ascii_serialization())
    {
        return Err(denied());
    }
    // Metadata authorization never admits a generation POST, even to the
    // credential-bearing origin. Keep this boundary common to all adapters.
    if models && (method != "GET" || body.is_some()) || !models && method != "POST" {
        return Err(denied());
    }
    if !["POST", "GET"].contains(&method.as_str()) || method == "GET" && body.is_some() {
        return Err(denied());
    }
    let mut map = http::header::HeaderMap::new();
    for (key, value) in headers {
        let name = http::header::HeaderName::from_bytes(key.as_bytes()).map_err(|_| denied())?;
        if !["content-type", "accept"].contains(&name.as_str()) {
            return Err(denied());
        }
        map.insert(
            name,
            http::header::HeaderValue::from_str(&value).map_err(|_| denied())?,
        );
    }
    for (name, value) in &bundle.manifest.headers {
        map.insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| denied())?,
            http::header::HeaderValue::from_str(value).map_err(|_| denied())?,
        );
    }

    let mut body = body.unwrap_or_default();
    if body.len() > 512 * 1024 {
        let mut error = Error::invalid();
        error.reason = Some("input_limit");
        return Err(error);
    }
    if let BrokerRouting::OpenRouter { only } = &bundle.manifest.routing {
        if !models && method == "POST" && target.path() == "/api/v1/chat/completions" {
            let mut request: serde_json::Value =
                serde_json::from_slice(&body).map_err(|_| denied())?;
            let object = request.as_object_mut().ok_or_else(denied)?;
            object.insert(
                "provider".into(),
                serde_json::json!({"only":only,"allow_fallbacks":false}),
            );
            body = serde_json::to_vec(&request).map_err(|_| denied())?;
        } else if !(method == "GET" && models && model_catalog_allowed(&target, only)) {
            return Err(denied());
        }
    }
    let placement = match &bundle.manifest.auth {
        AuthProfile::None => crate::llm::CredentialPlacement::None,
        AuthProfile::Bearer => crate::llm::CredentialPlacement::Bearer,
        AuthProfile::Header { name } => crate::llm::CredentialPlacement::Header(name.clone()),
        AuthProfile::Query { name } => crate::llm::CredentialPlacement::Query(name.clone()),
    };
    let transport = crate::llm::AuthorizedTransport::new(input.clone(), placement)?;
    let mut request = http::Request::builder()
        .method(method.as_str())
        .uri(target.as_str())
        .body(bytes::Bytes::from(body))
        .map_err(|_| denied())?;
    *request.headers_mut() = map;
    transport.send(request).await
}

/// The client capability binds requests to a host-owned invocation; plugins
/// cannot supply another job ID, grant, purpose or credential.
pub struct Broker {
    pub bundle: Bundle,
    pub input: Arc<Invocation>,
    pub calls: AtomicUsize,
    pub active: Arc<tokio::sync::Semaphore>,
}
impl Broker {
    pub async fn request(
        &self,
        request: HttpRequest,
    ) -> Result<(crate::llm::Response, tokio::sync::OwnedSemaphorePermit)> {
        let _active = self
            .active
            .clone()
            .try_acquire_owned()
            .map_err(|_| denied())?;
        if self.calls.fetch_add(1, Ordering::Relaxed) >= 8 {
            return Err(denied());
        }
        let response = send(self.bundle.clone(), self.input.clone(), request).await?;
        Ok((response, _active))
    }
}

fn model_catalog_allowed(target: &url::Url, only: &[String]) -> bool {
    target.path() == "/api/v1/models"
        && target.query_pairs().collect::<Vec<_>>()
            == vec![("providers".into(), only.join(",").into())]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_catalog_cannot_broaden_the_approved_route() {
        let only = vec!["openai".into()];
        for query in [
            "",
            "?providers=anthropic",
            "?providers=openai,anthropic",
            "?providers=openai&providers=anthropic",
        ] {
            let target =
                url::Url::parse(&format!("https://openrouter.ai/api/v1/models{query}")).unwrap();
            assert!(!model_catalog_allowed(&target, &only));
        }
        let target =
            url::Url::parse("https://openrouter.ai/api/v1/models?providers=openai").unwrap();
        assert!(model_catalog_allowed(&target, &only));
        let only = vec!["openai".into(), "anthropic".into()];
        let target =
            url::Url::parse("https://openrouter.ai/api/v1/models?providers=openai%2Canthropic")
                .unwrap();
        assert!(model_catalog_allowed(&target, &only));
    }
}

#[cfg(test)]
#[path = "broker/tests.rs"]
mod authorization_tests;
