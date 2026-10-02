//! Only the host adds credentials. Redirects and unapproved origins are rejected.
use super::manifest::Bundle;
use base64::{Engine, engine::general_purpose::STANDARD};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Result, llm::Invocation},
};
use ene_protocol::provider::{AuthProfile, BrokerRouting, HostMessage, PluginMessage};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
fn denied() -> Error {
    Error::new(ErrorKind::PermissionRequired)
}
pub async fn send(
    bundle: Bundle,
    input: std::sync::Arc<Invocation>,
    request: PluginMessage,
    out: mpsc::Sender<Result<HostMessage>>,
) -> Result<()> {
    let PluginMessage::HttpRequest {
        request_id,
        http_id,
        method,
        url,
        headers,
        body_base64,
    } = request
    else {
        return Err(denied());
    };
    if request_id != input.id
        || http_id.is_empty()
        || http_id.len() > 100
        || http_id.chars().any(char::is_control)
    {
        return Err(denied());
    }
    input.authorization.check().await?;
    let base = url::Url::parse(&input.connection.endpoint).map_err(|_| denied())?;
    let mut target = url::Url::parse(&url).map_err(|_| denied())?;
    if target.scheme() != base.scheme()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || target.origin() != base.origin()
        || !bundle
            .manifest
            .destinations
            .contains(&target.origin().ascii_serialization())
        || !(target.path() == base.path().trim_end_matches('/')
            || target
                .path()
                .starts_with(&format!("{}/", base.path().trim_end_matches('/'))))
    {
        return Err(denied());
    }
    if !["POST", "GET"].contains(&method.as_str()) || method == "GET" && body_base64.is_some() {
        return Err(denied());
    }
    let mut map = reqwest::header::HeaderMap::new();
    for (key, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|_| denied())?;
        if !["content-type", "accept"].contains(&name.as_str()) {
            return Err(denied());
        }
        map.insert(
            name,
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| denied())?,
        );
    }
    for (name, value) in &bundle.manifest.headers {
        map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| denied())?,
            reqwest::header::HeaderValue::from_str(value).map_err(|_| denied())?,
        );
    }
    let secret = input.secret.as_ref();
    match &bundle.manifest.auth {
        AuthProfile::None => {}
        AuthProfile::Bearer => {
            let value = secret.ok_or_else(denied)?;
            map.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", value.value))
                    .map_err(|_| denied())?,
            );
        }
        AuthProfile::Header { name } => {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| denied())?,
                reqwest::header::HeaderValue::from_str(&secret.ok_or_else(denied)?.value)
                    .map_err(|_| denied())?,
            );
        }
        AuthProfile::Query { name } => {
            if target.query_pairs().any(|(k, _)| k == name.as_str()) {
                return Err(denied());
            }
            target
                .query_pairs_mut()
                .append_pair(name, &secret.ok_or_else(denied)?.value);
        }
    }
    let mut body = body_base64
        .map(|s| STANDARD.decode(s).map_err(|_| denied()))
        .transpose()?
        .unwrap_or_default();
    if body.len() > 512 * 1024 {
        return Err(denied());
    }
    if let BrokerRouting::OpenRouter { only } = &bundle.manifest.routing {
        if method == "POST" && target.path() == "/api/v1/chat/completions" {
            let mut request: serde_json::Value =
                serde_json::from_slice(&body).map_err(|_| denied())?;
            let object = request.as_object_mut().ok_or_else(denied)?;
            object.insert(
                "provider".into(),
                serde_json::json!({"only":only,"allow_fallbacks":false}),
            );
            body = serde_json::to_vec(&request).map_err(|_| denied())?;
        } else if !(method == "GET" && target.path() == "/api/v1/models") {
            return Err(denied());
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(150))
        .build()
        .map_err(|_| Error::unavailable())?;
    // Final check immediately before transferring data, after validation and credential injection.
    input.authorization.check().await?;
    let mut response = client
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| denied())?,
            target,
        )
        .headers(map)
        .body(body)
        .send()
        .await
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    if response.status().is_redirection() {
        return Err(denied());
    }
    let status = response.status().as_u16();
    let mut headers = BTreeMap::new();
    if let Some(v) = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
    {
        headers.insert("content-type".into(), v.into());
    }
    let mut bytes = 0usize;
    while let Some(data) = response
        .chunk()
        .await
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?
    {
        input.authorization.check().await?;
        bytes += data.len();
        if bytes > 8 * 1024 * 1024 {
            return Err(Error::new(ErrorKind::ExternalFailure));
        }
        for chunk in data.chunks(48 * 1024) {
            out.send(Ok(HostMessage::HttpResponse {
                request_id: input.id.clone(),
                http_id: http_id.clone(),
                status,
                headers: headers.clone(),
                data_base64: STANDARD.encode(chunk),
                done: false,
            }))
            .await
            .map_err(|_| Error::unavailable())?;
        }
    }
    input.authorization.check().await?;
    out.send(Ok(HostMessage::HttpResponse {
        request_id: input.id.clone(),
        http_id,
        status,
        headers,
        data_base64: String::new(),
        done: true,
    }))
    .await
    .map_err(|_| Error::unavailable())?;
    Ok(())
}
