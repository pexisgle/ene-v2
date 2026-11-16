#![allow(clippy::result_large_err, clippy::manual_async_fn)]

//! A provider plugin as a rig completion model.
//!
//! The host keeps rig's OpenAI wire and gives it an in-process HTTP client.
//! The client turns rig's encoded chat request into one Extism operation; the
//! guest forwards it through the host's broker and returns the provider's
//! bytes. SSE parsing, tool-call assembly and finish reasons stay in rig.
use super::super::providers::{BrokerServices, Running};
use crate::host::error::HostError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ene_plugin::{Event as PluginEvent, HttpChunk, HttpRequest, Operation as PluginOperation};
use ene_plugin_runtime::service::HttpBroker;
use http::header::HeaderName;
use rig_core::{
    driver::DynModel,
    http_client::{
        BoxedStream, Bytes, Error as HttpError, HeaderMap, HeaderValue, HttpClientExt, LazyBody,
        MultipartForm, Request, Response, StatusCode, StreamingResponse,
    },
    operation::Completion,
    providers::openai::OpenAIConfig,
};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio::sync::mpsc;

/// The key rig's OpenAI wire sends. The broker replaces it with the host's
/// credential, and the client strips it before the guest sees the request.
const BROKER_KEY: &str = "ene-broker";

/// Headers the broker owns. The client removes them before the guest, and the
/// broker refuses them if a guest sets them anyway.
const HOST_OWNED: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
];

/// A plugin-backed completion model, erased to rig's operation.
pub(crate) fn completion(
    provider: Arc<Running>,
    model: String,
    broker: Arc<HttpBroker>,
) -> DynModel<Completion> {
    let endpoint = provider.provider().endpoint.clone();
    OpenAIConfig::new(BROKER_KEY)
        .with_base_url(endpoint)
        .connect(GuestClient { provider, broker })
        .chat(model)
        .erase()
}

/// rig's HTTP transport, answered by one Extism invocation.
#[derive(Clone)]
struct GuestClient {
    provider: Arc<Running>,
    broker: Arc<HttpBroker>,
}

type Invocation = Pin<Box<dyn Future<Output = Result<serde_json::Value, HostError>> + Send>>;

struct Exchange {
    invocation: Invocation,
    events: mpsc::Receiver<PluginEvent>,
    broker: Arc<HttpBroker>,
    handles: Vec<String>,
    status: Option<StatusCode>,
    headers: Option<HeaderMap>,
    pending: VecDeque<Bytes>,
    done: bool,
    invocation_done: bool,
}

impl Exchange {
    fn accept(&mut self, chunk: HttpChunk) -> Result<(), HttpError> {
        if self.done {
            return Err(protocol("a chunk arrived after the terminal chunk"));
        }
        if self.status.is_none() {
            let status = StatusCode::from_u16(chunk.status)
                .map_err(|_| protocol("the provider returned an invalid status"))?;
            let mut headers = HeaderMap::new();
            for (name, value) in &chunk.headers {
                let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
                    continue;
                };
                if matches!(name.as_str(), "set-cookie" | "transfer-encoding") {
                    continue;
                }
                if let Ok(value) = HeaderValue::from_str(value) {
                    headers.insert(name, value);
                }
            }
            self.status = Some(status);
            self.headers = Some(headers);
        }
        if !chunk.handle.is_empty() && !self.handles.contains(&chunk.handle) {
            self.handles.push(chunk.handle.clone());
        }
        if !chunk.data_base64.is_empty() {
            let bytes = STANDARD
                .decode(&chunk.data_base64)
                .map_err(|_| protocol("the provider chunk was not base64"))?;
            if !bytes.is_empty() {
                self.pending.push_back(Bytes::from(bytes));
            }
        }
        if chunk.done {
            self.done = true;
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<Bytes>, HttpError> {
        loop {
            if let Some(bytes) = self.pending.pop_front() {
                return Ok(Some(bytes));
            }
            if self.done {
                if !self.invocation_done {
                    self.invocation_done = true;
                    (&mut self.invocation).await.map_err(HttpError::instance)?;
                }
                return Ok(None);
            }
            if self.invocation_done {
                match self.events.recv().await {
                    Some(PluginEvent::Http { chunk }) => self.accept(chunk)?,
                    Some(_) => return Err(protocol("the plugin emitted a non-HTTP event")),
                    None => {
                        return Err(protocol("the plugin ended without a terminal chunk"));
                    }
                }
                continue;
            }
            tokio::select! {
                event = self.events.recv() => match event {
                    Some(PluginEvent::Http { chunk }) => self.accept(chunk)?,
                    Some(_) => return Err(protocol("the plugin emitted a non-HTTP event")),
                    None => {
                        self.invocation_done = true;
                        (&mut self.invocation)
                            .await
                            .map_err(HttpError::instance)?;
                        if !self.done {
                            return Err(protocol("the plugin ended without a terminal chunk"));
                        }
                    }
                },
                result = &mut self.invocation => {
                    self.invocation_done = true;
                    result.map_err(HttpError::instance)?;
                }
            }
        }
    }

    async fn start(&mut self) -> Result<(StatusCode, HeaderMap), HttpError> {
        while self.status.is_none() {
            match self.next().await? {
                Some(bytes) => self.pending.push_front(bytes),
                None => break,
            }
        }
        let status = self
            .status
            .ok_or_else(|| protocol("the plugin sent no response line"))?;
        Ok((status, self.headers.take().unwrap_or_default()))
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        for handle in &self.handles {
            let _ = self.broker.close(handle);
        }
    }
}

impl GuestClient {
    fn exchange(&self, parts: &http::request::Parts, body: Bytes) -> Result<Exchange, HttpError> {
        let endpoint = self.provider.provider().endpoint.clone();
        let url = if parts.uri.scheme().is_some() {
            parts.uri.to_string()
        } else {
            let base = url::Url::parse(&endpoint)
                .map_err(|_| protocol("the provider endpoint is not a URL"))?;
            base.join(parts.uri.path_and_query().map_or("/", |path| path.as_str()))
                .map_err(|_| protocol("the provider endpoint and route do not join"))?
                .to_string()
        };
        let mut headers = BTreeMap::new();
        for (name, value) in &parts.headers {
            if HOST_OWNED.contains(&name.as_str()) {
                continue;
            }
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().to_owned(), value.to_owned());
            }
        }
        let request = HttpRequest {
            method: parts.method.as_str().to_owned(),
            url,
            headers,
            body_base64: Some(STANDARD.encode(&body)),
            stream: true,
        };
        let (sender, events) = mpsc::channel(64);
        let provider = Arc::clone(&self.provider);
        let services =
            BrokerServices::new(Arc::clone(&self.broker), provider.provider().id.clone());
        let invocation: Invocation = Box::pin(async move {
            provider
                .invoke_streaming(PluginOperation::GenerateHttp { request }, services, sender)
                .await
        });
        Ok(Exchange {
            invocation,
            events,
            broker: Arc::clone(&self.broker),
            handles: Vec::new(),
            status: None,
            headers: None,
            pending: VecDeque::new(),
            done: false,
            invocation_done: false,
        })
    }
}

impl HttpClientExt for GuestClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, HttpError>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let exchange = self.exchange(&parts, body);
        async move {
            let mut exchange = exchange?;
            let (status, headers) = exchange.start().await?;
            let mut body = Vec::new();
            while let Some(bytes) = exchange.next().await? {
                body.extend_from_slice(&bytes);
            }
            let mut response = Response::new(
                Box::pin(async move { Ok(U::from(Bytes::from(body))) }) as LazyBody<U>,
            );
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            Ok(response)
        }
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, HttpError>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        async { unimplemented!("chat completions does not use multipart") }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse, HttpError>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let exchange = self.exchange(&parts, body);
        async move {
            let mut exchange = exchange?;
            let (status, headers) = exchange.start().await?;
            let stream = async_stream::try_stream! {
                while let Some(bytes) = exchange.next().await? {
                    yield bytes;
                }
            };
            let mut response = Response::new(Box::pin(stream) as BoxedStream);
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            Ok(response)
        }
    }
}

fn protocol(message: &str) -> HttpError {
    HttpError::instance(std::io::Error::other(message.to_owned()))
}
