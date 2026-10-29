//! Authorization, credential attachment and sending share one private boundary.
use super::{CredentialPlacement, Invocation};
use crate::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use bytes::Bytes;
use std::{sync::Arc, time::Duration};

pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct AuthorizedTransport {
    client: reqwest::Client,
    invocation: Arc<Invocation>,
    placement: CredentialPlacement,
    max_response_bytes: usize,
    timeout: Duration,
}

impl AuthorizedTransport {
    pub fn new(invocation: Arc<Invocation>, placement: CredentialPlacement) -> Result<Self> {
        let client = http_client()?;
        let timeout = invocation.operation().spec().timeout;
        Ok(Self {
            client,
            timeout,
            invocation,
            placement,
            max_response_bytes: MAX_RESPONSE_BYTES,
        })
    }

    /// Public metadata has a separate limit and cannot carry credentials or a body.
    pub fn public_catalog(invocation: Arc<Invocation>) -> Result<Self> {
        let mut transport = Self::new(invocation, CredentialPlacement::None)?;
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
        self.invocation
            .authorize_outbound(&mut request, &self.placement)
            .await?;
        let (parts, body) = request.into_parts();
        // This is the actual send boundary, including Rig's lazy streaming requests.
        let response = self
            .client
            .request(parts.method, parts.uri.to_string())
            .timeout(self.timeout)
            .headers(parts.headers)
            .body(body)
            .send()
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
            invocation: self.invocation.clone(),
            bytes: 0,
            max_bytes: self.max_response_bytes,
        })
    }
}

pub struct Response {
    response: reqwest::Response,
    invocation: Arc<Invocation>,
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
        self.invocation.authorization().check().await?;
        let chunk = self.response.chunk().await.map_err(|_| external())?;
        self.invocation.authorization().check().await?;
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
