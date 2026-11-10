//! Authorized HTTP transport enforcing destination origin, live permit and 8 MiB stream bounds.
use super::origin;
use crate::host::db::{Result, unavailable};
use bytes::Bytes;
use futures_util::StreamExt;
use rig_http::{
    http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse},
    wasm_compat::WasmCompatSend,
};
use rig_reqwest::ReqwestClient;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
/// How long a stream may go without sending anything before it is abandoned.
const READ_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub(super) struct Authorized {
    client: ReqwestClient,
    allowed: Arc<AtomicBool>,
    destination: String,
}

fn denied() -> http_client::Error {
    http_client::Error::instance(std::io::Error::other(
        "model request denied or transport failed",
    ))
}

impl Authorized {
    pub(super) fn new(allowed: Arc<AtomicBool>, destination: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            // A generation may stream for minutes: only silence times out.
            // A whole-request deadline would cut a long answer off.
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| unavailable("cannot build model transport"))?;
        Ok(Self {
            client: ReqwestClient::from(client),
            allowed,
            destination,
        })
    }

    fn permits(&self, uri: &http::Uri) -> bool {
        self.allowed.load(Ordering::Acquire)
            && origin(&uri.to_string()).as_deref() == Some(&self.destination)
    }
}

impl HttpClientExt for Authorized {
    fn send<T, U>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            if !this.permits(request.uri()) {
                return Err(denied());
            }
            this.client.send(request).await.map_err(|error| {
                // The provider's reason never reaches the client: the failure
                // is reported as "generation failed", so it is logged here.
                tracing::warn!(%error, "model request failed");
                denied()
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _: http::Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        async { Err(denied()) }
    }

    fn send_streaming<T>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            if !this.permits(request.uri()) {
                return Err(denied());
            }
            let response = this.client.send_streaming(request).await.map_err(|error| {
                tracing::warn!(%error, "model stream failed");
                denied()
            })?;
            let (parts, mut body) = response.into_parts();
            let bounded = async_stream::try_stream! {
                let mut size = 0usize;
                while let Some(chunk) = body.next().await {
                    if !this.allowed.load(Ordering::Acquire) {
                        Err(denied())?;
                    }
                    let bytes = chunk.map_err(|error| {
                        tracing::warn!(%error, "model stream ended early");
                        denied()
                    })?;
                    size = size.saturating_add(bytes.len());
                    if size > MAX_STREAM_BYTES {
                        Err(denied())?;
                    }
                    yield bytes;
                }
            };
            Ok(http::Response::from_parts(
                parts,
                Box::pin(bounded) as http_client::BoxedStream,
            ))
        }
    }
}
