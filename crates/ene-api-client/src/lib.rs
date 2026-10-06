//! Typed REST API v1 client with authenticated, bounded event subscriptions.
//! SyncedSession owns bounded snapshots, notification application and history
//! reconciliation even when callers cancel a wait. Mutations require explicit retries.
mod events;
pub mod session;
pub use session::{SessionIdentity, SyncedSession};
#[cfg(test)]
mod session_tests;
pub use ene_api_types as types;
use ene_api_types::sync::Replica;
use ene_api_types::{
    host::*,
    provider::{Model, ProviderInfo},
};
pub use events::{EventStream, ServerEvent};
use reqwest::{
    Method, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid API endpoint or Bearer token")]
    InvalidConnection,
    #[error("invalid idempotency key")]
    InvalidKey,
    #[error("request transport failed")]
    Transport,
    #[error("authentication required")]
    Unauthorized,
    #[error("{0}")]
    Api(ApiFailure),
    #[error("invalid or oversized API response")]
    Protocol,
    #[error("subscription must be resynchronized")]
    ResyncRequired,
    #[error("loaded conversation exceeds the client memory limit")]
    ResourceLimit,
    #[error("client command queue is full")]
    Busy,
}
#[derive(Clone, Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub body: ApiErrorBody,
}
impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.body.error.message, self.body.error.code)
    }
}
/// Explicit retry identity, checked against the server's 1..=200 byte contract.
#[derive(Clone, Debug)]
pub struct IdempotencyKey(String);
impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 200
            || value.bytes().any(|b| !(0x20..=0x7e).contains(&b))
        {
            return Err(Error::InvalidKey);
        }
        Ok(Self(value))
    }
}

#[derive(Clone)]
pub struct ApiClient(Arc<Inner>);
struct Inner {
    http: reqwest::Client,
    endpoint: Url,
    bearer: HeaderValue,
}
// Credentials and request URLs are intentionally omitted from Debug/errors.
impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient").finish_non_exhaustive()
    }
}
impl ApiClient {
    /// Accepts HTTP only for literal loopback addresses, or verified HTTPS.
    /// Redirects and proxy discovery are disabled, so credentials cannot follow
    /// redirects or be sent through an ambient proxy for local host access.
    pub fn new(connection: &RuntimeConnection) -> Result<Self, Error> {
        let endpoint = Url::parse(&connection.endpoint).map_err(|_| Error::InvalidConnection)?;
        let loopback = match endpoint.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
            || endpoint.host().is_none()
            || endpoint.path() != "/v1"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || connection.token.is_empty()
            || connection
                .token
                .bytes()
                .any(|b| !(0x21..=0x7e).contains(&b))
        {
            return Err(Error::InvalidConnection);
        }
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", connection.token))
            .map_err(|_| Error::InvalidConnection)?;
        bearer.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self(Arc::new(Inner {
            http,
            endpoint,
            bearer,
        })))
    }
    fn url(&self, segments: &[&str]) -> Result<Url, Error> {
        let mut url = self.0.endpoint.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| Error::InvalidConnection)?;
        for segment in segments {
            // IDs are opaque; no dot-segment normalization or empty path entries.
            if segment.is_empty() || *segment == "." || *segment == ".." {
                return Err(Error::Protocol);
            }
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<&impl Serialize>,
        key: Option<&IdempotencyKey>,
        timeout: Duration,
    ) -> Result<T, Error> {
        let mut request = self
            .0
            .http
            .request(method, url)
            .header(AUTHORIZATION, self.0.bearer.clone())
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", &key.0);
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let body = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            return Err(Error::Api(ApiFailure {
                status: status.as_u16(),
                body,
            }));
        }
        if status == StatusCode::NO_CONTENT {
            serde_json::from_str("null").map_err(|_| Error::Protocol)
        } else {
            serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
        }
    }
    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, Error> {
        self.request(Method::GET, url, None::<&()>, None, Duration::from_secs(5))
            .await
    }
    async fn mutate<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&impl Serialize>,
        key: Option<&IdempotencyKey>,
    ) -> Result<T, Error> {
        self.request(method, self.url(path)?, body, key, Duration::from_secs(10))
            .await
    }
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<Snapshot, Error> {
        let mut url = self.url(&["snapshot"])?;
        if let Some(id) = subscription {
            url.query_pairs_mut().append_pair("connection_id", id);
        }
        self.get(url).await
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        self.get(self.url(&["companions"])?).await
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        self.get(self.url(&["companions", id])?).await
    }
    pub async fn patch_companion(
        &self,
        id: &str,
        input: &CompanionPatch,
    ) -> Result<Companion, Error> {
        self.mutate(Method::PATCH, &["companions", id], Some(input), None)
            .await
    }
    pub async fn messages(
        &self,
        companion: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Page<Message>, Error> {
        let mut url = self.url(&["companions", companion, "messages"])?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(limit) = limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        self.get(url).await
    }
    pub async fn message(&self, companion: &str, id: &str) -> Result<Message, Error> {
        self.get(self.url(&["companions", companion, "messages", id])?)
            .await
    }
    pub async fn send_message(
        &self,
        companion: &str,
        input: &MessageInput,
        key: &IdempotencyKey,
    ) -> Result<MessageAcceptance, Error> {
        self.mutate(
            Method::POST,
            &["companions", companion, "messages"],
            Some(input),
            Some(key),
        )
        .await
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        self.get(self.url(&["settings"])?).await
    }
    pub async fn patch_settings(&self, input: &SettingsPatch) -> Result<Settings, Error> {
        self.mutate(Method::PATCH, &["settings"], Some(input), None)
            .await
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<DataTransferGrant>, Error> {
        let mut url = self.url(&["grants"])?;
        if let Some(id) = companion {
            url.query_pairs_mut().append_pair("companion_id", id);
        }
        self.get(url).await
    }
    pub async fn create_grant(
        &self,
        input: &GrantInput,
        key: &IdempotencyKey,
    ) -> Result<GrantResult, Error> {
        self.mutate(Method::POST, &["grants"], Some(input), Some(key))
            .await
    }
    pub async fn patch_grant(&self, id: &str, input: &GrantPatch) -> Result<GrantResult, Error> {
        self.mutate(Method::PATCH, &["grants", id], Some(input), None)
            .await
    }
    pub async fn revoke_grant(&self, id: &str) -> Result<(), Error> {
        self.mutate(Method::DELETE, &["grants", id], None::<&()>, None)
            .await
    }
    pub async fn register_credentials(
        &self,
        id: &str,
        input: &CredentialInput,
    ) -> Result<CredentialResult, Error> {
        self.mutate(Method::PUT, &["credentials", id], Some(input), None)
            .await
    }
    pub async fn revoke_credentials(&self, id: &str) -> Result<(), Error> {
        self.mutate(Method::DELETE, &["credentials", id], None::<&()>, None)
            .await
    }
    pub async fn check(
        &self,
        id: &str,
        input: &CheckInput,
        key: &IdempotencyKey,
    ) -> Result<CheckResult, Error> {
        self.request(
            Method::POST,
            self.url(&["connections", id, "check"])?,
            Some(input),
            Some(key),
            Duration::from_secs(95),
        )
        .await
    }
    pub async fn models(&self, id: &str, companion: Option<&str>) -> Result<Vec<Model>, Error> {
        let mut url = self.url(&["connections", id, "models"])?;
        if let Some(id) = companion {
            url.query_pairs_mut().append_pair("companion_id", id);
        }
        self.request(Method::GET, url, None::<&()>, None, Duration::from_secs(35))
            .await
    }
    pub async fn providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.get(self.url(&["providers"])?).await
    }
    pub async fn reload_providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.mutate(Method::POST, &["providers", "reload"], None::<&()>, None)
            .await
    }
    pub async fn enable_provider(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Vec<ProviderInfo>, Error> {
        self.mutate(
            Method::PUT,
            &["providers", id, "enabled"],
            Some(&ProviderEnabled { enabled }),
            None,
        )
        .await
    }
    pub async fn openapi(&self) -> Result<serde_json::Value, Error> {
        self.get(self.url(&["openapi.json"])?).await
    }
}

#[cfg(test)]
mod tests;
