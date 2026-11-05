//! Authorization, credential attachment and sending share one private boundary.
use super::{CredentialPlacement, Invocation, Outbound, TransportScope};
use crate::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio_util::context::TokioContext;

pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct AuthorizedTransport {
    client: reqwest::Client,
    outbound: Arc<Outbound>,
    placement: CredentialPlacement,
    scope: TransportScope,
    runtime: tokio::runtime::Handle,
    max_response_bytes: usize,
    timeout: Duration,
}

impl AuthorizedTransport {
    pub fn new(invocation: Arc<Invocation>, placement: CredentialPlacement) -> Result<Self> {
        Self::for_outbound(invocation.outbound(), placement)
    }

    /// The same boundary for a request the host does not own: the Rig
    /// execution kernel sends its own requests through this transport.
    pub(crate) fn for_outbound(
        outbound: Arc<Outbound>,
        placement: CredentialPlacement,
    ) -> Result<Self> {
        let client = http_client()?;
        let timeout = outbound.operation().spec().timeout;
        let max_response_bytes = MAX_RESPONSE_BYTES;
        // A Rig task pool polls this future on its own thread, where no Tokio
        // runtime is current. Capture the host's runtime here so reqwest's
        // I/O is driven by the host runtime, not by a second implicit one.
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::unavailable())?;
        Ok(Self {
            client,
            timeout,
            outbound,
            placement,
            runtime,
            scope: TransportScope::Provider,
            max_response_bytes,
        })
    }

    /// Public metadata has a separate limit and cannot carry credentials or a body.
    pub fn public_catalog(invocation: Arc<Invocation>, source: &str) -> Result<Self> {
        let mut transport = Self::new(invocation, CredentialPlacement::None)?;
        transport.scope =
            TransportScope::Catalog(url::Url::parse(source).map_err(|_| Error::invalid())?);
        transport.timeout = Duration::from_secs(10);
        transport.max_response_bytes = 16 * 1024 * 1024;
        Ok(transport)
    }

    pub async fn send(&self, mut request: http::Request<Bytes>) -> Result<Response> {
        if request.body().len() > MAX_REQUEST_BYTES {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("input_limit"),
            });
        }
        self.outbound
            .authorize_outbound(&mut request, &self.placement, &self.scope)
            .await?;
        let (parts, body) = request.into_parts();
        // This is the actual send boundary, including Rig's lazy streaming
        // requests. reqwest builds its deadline with `tokio::time::sleep`
        // while the request builder is constructed, so the builder and the
        // send both run inside the captured host runtime: a Rig task pool
        // has no runtime of its own to fall back on.
        let client = self.client.clone();
        let timeout = self.timeout;
        let runtime = self.runtime.clone();
        let response = TokioContext::new(
            async move {
                client
                    .request(parts.method, parts.uri.to_string())
                    .timeout(timeout)
                    .headers(parts.headers)
                    .body(body)
                    .send()
                    .await
            },
            runtime,
        )
        .await
        .map_err(|_| external())?;
        if response.status().is_redirection() {
            return Err(Error::new(ErrorKind::PermissionRequired));
        }
        if response
            .content_length()
            .is_some_and(|size| size > self.max_response_bytes as u64)
        {
            return Err(external());
        }
        Ok(Response {
            response,
            outbound: self.outbound.clone(),
            runtime: self.runtime.clone(),
            bytes: 0,
            max_bytes: self.max_response_bytes,
        })
    }
}

pub struct Response {
    response: reqwest::Response,
    outbound: Arc<Outbound>,
    runtime: tokio::runtime::Handle,
    bytes: usize,
    max_bytes: usize,
}
impl Response {
    pub fn status(&self) -> http::StatusCode {
        self.response.status()
    }
    pub fn headers(&self) -> &http::HeaderMap {
        self.response.headers()
    }
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.outbound.authorization().check().await?;
        let chunk = TokioContext::new(self.response.chunk(), self.runtime.clone())
            .await
            .map_err(|_| external())?;
        self.outbound.authorization().check().await?;
        if let Some(chunk) = &chunk {
            self.bytes = self.bytes.saturating_add(chunk.len());
            if self.bytes > self.max_bytes {
                return Err(external());
            }
        }
        Ok(chunk)
    }
    pub async fn bytes(mut self) -> Result<Bytes> {
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body.into())
    }
}
fn external() -> Error {
    Error::new(ErrorKind::ProviderFailure)
}

fn http_client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .user_agent(concat!("ene/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|_| Error::unavailable())
        })
        .clone()
}

mod rig;
