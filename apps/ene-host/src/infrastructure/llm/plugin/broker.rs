//! Only the host adds credentials. Redirects and unapproved origins are rejected.
use super::manifest::Bundle;
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Result, llm::Invocation},
};
use ene_ipc::provider::{
    AuthProfile, BrokerRouting, HttpBroker, HttpRequest, HttpResponsePart, ProviderError, Sender,
};
use std::collections::BTreeMap;
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
    out: Sender<HttpResponsePart>,
) -> Result<()> {
    let models = input.operation() == ene_core::domain::OperationKind::ModelList;
    let HttpRequest {
        method,
        url,
        headers,
        body,
    } = request;
    input.authorization().check().await?;
    let base = url::Url::parse(&input.connection().endpoint).map_err(|_| denied())?;
    let target = url::Url::parse(&url).map_err(|_| denied())?;
    let public = target.origin() != base.origin();
    if !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || !bundle
            .manifest
            .destinations
            .contains(&target.origin().ascii_serialization())
        || !public
            && !(target.path() == base.path().trim_end_matches('/')
                || target
                    .path()
                    .starts_with(&format!("{}/", base.path().trim_end_matches('/'))))
    {
        return Err(denied());
    }
    // Metadata authorization never admits a generation POST, even to the
    // credential-bearing origin. Keep this boundary common to all adapters.
    if models && (method != "GET" || body.is_some()) || public && !models {
        return Err(denied());
    }
    if !["POST", "GET"].contains(&method.as_str()) || method == "GET" && body.is_some() {
        return Err(denied());
    }
    let mut map = ene_outbound::http::header::HeaderMap::new();
    for (key, value) in headers {
        let name = ene_outbound::http::header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| denied())?;
        if !["content-type", "accept"].contains(&name.as_str()) {
            return Err(denied());
        }
        map.insert(
            name,
            ene_outbound::http::header::HeaderValue::from_str(&value).map_err(|_| denied())?,
        );
    }
    if !public {
        for (name, value) in &bundle.manifest.headers {
            map.insert(
                ene_outbound::http::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| denied())?,
                ene_outbound::http::header::HeaderValue::from_str(value).map_err(|_| denied())?,
            );
        }
    }

    let mut body = body.unwrap_or_default();
    if body.len() > 512 * 1024 {
        let mut error = Error::invalid();
        error.reason = Some("input_limit");
        return Err(error);
    }
    if !public && let BrokerRouting::OpenRouter { only } = &bundle.manifest.routing {
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
        AuthProfile::None => ene_core::ports::llm::CredentialPlacement::None,
        AuthProfile::Bearer => ene_core::ports::llm::CredentialPlacement::Bearer,
        AuthProfile::Header { name } => {
            ene_core::ports::llm::CredentialPlacement::Header(name.clone())
        }
        AuthProfile::Query { name } => {
            ene_core::ports::llm::CredentialPlacement::Query(name.clone())
        }
    };
    let transport = ene_outbound::AuthorizedTransport::new(input.clone(), placement)?;
    let mut request = ene_outbound::http::Request::builder()
        .method(method.as_str())
        .uri(target.as_str())
        .body(ene_outbound::Bytes::from(body))
        .map_err(|_| denied())?;
    *request.headers_mut() = map;
    let mut response = transport.send(request).await?;
    let status = response.status().as_u16();
    let mut headers = BTreeMap::new();
    if let Some(v) = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
    {
        headers.insert("content-type".into(), v.into());
    }
    out.send(HttpResponsePart::Head { status, headers })
        .await
        .map_err(|_| Error::unavailable())?
        .await
        .map_err(|_| Error::unavailable())?;
    let mut bytes = 0usize;
    while let Some(data) = response
        .chunk()
        .await
        .map_err(|_| Error::new(ErrorKind::ProviderFailure))?
    {
        input.authorization().check().await?;
        bytes += data.len();
        if bytes > 8 * 1024 * 1024 {
            return Err(Error::new(ErrorKind::ProviderFailure));
        }
        out.send(HttpResponsePart::Data(data.to_vec()))
            .await
            .map_err(|_| Error::unavailable())?
            .await
            .map_err(|_| Error::unavailable())?;
    }
    input.authorization().check().await?;
    Ok(())
}

/// The client capability binds requests to a host-owned invocation; plugins
/// cannot supply another job ID, grant, purpose or credential.
pub struct Broker {
    pub bundle: Bundle,
    pub input: Arc<Invocation>,
    pub calls: AtomicUsize,
    pub active: tokio::sync::Semaphore,
}
impl HttpBroker for Broker {
    async fn request(
        &self,
        request: HttpRequest,
        out: Sender<HttpResponsePart>,
    ) -> std::result::Result<(), ProviderError> {
        let _active = self
            .active
            .try_acquire()
            .map_err(|_| ProviderError::PermissionDenied)?;
        if self.calls.fetch_add(1, Ordering::Relaxed) >= 8 {
            return Err(ProviderError::PermissionDenied);
        }
        send(self.bundle.clone(), self.input.clone(), request, out)
            .await
            .map_err(|error| match error.kind {
                ErrorKind::PermissionRequired => ProviderError::PermissionDenied,
                ErrorKind::InvalidRequest => ProviderError::InvalidInput,
                ErrorKind::Unavailable => ProviderError::Unavailable,
                _ => ProviderError::Failed,
            })
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
