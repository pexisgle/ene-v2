//! External adapters receive this transport; the raw client is never exported.
mod rig;
pub use bytes::Bytes;
pub use ene_core::ports::llm::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Response};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{
        Result,
        llm::{CredentialPlacement, Invocation},
    },
};
pub use http;
use std::sync::Arc;
pub use url;

pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Rig-facing facade over Core's sealed signing and sending boundary.
#[derive(Clone)]
pub struct AuthorizedTransport(ene_core::ports::llm::AuthorizedTransport);
impl AuthorizedTransport {
    pub fn new(invocation: Arc<Invocation>, placement: CredentialPlacement) -> Result<Self> {
        ene_core::ports::llm::AuthorizedTransport::new(invocation, placement).map(Self)
    }
    pub fn public_catalog(invocation: Arc<Invocation>) -> Result<Self> {
        ene_core::ports::llm::AuthorizedTransport::public_catalog(invocation).map(Self)
    }
    pub async fn send(&self, request: http::Request<Bytes>) -> Result<Response> {
        self.0.send(request).await
    }
}
fn external() -> Error {
    Error::new(ErrorKind::ProviderFailure)
}

/// Tracks framing across HTTP chunks before Rig can buffer an oversized event.
#[derive(Default)]
struct EventBudget {
    event: usize,
    line_content: bool,
}
impl EventBudget {
    fn accept(&mut self, bytes: &[u8]) -> Result<()> {
        for byte in bytes {
            self.event += 1;
            if self.event > MAX_EVENT_BYTES {
                return Err(external());
            }
            if *byte == b'\n' {
                if !self.line_content {
                    self.event = 0;
                }
                self.line_content = false;
            } else if *byte != b'\r' {
                self.line_content = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
