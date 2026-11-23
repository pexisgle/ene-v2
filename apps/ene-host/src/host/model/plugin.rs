//! A provider plugin as a rig completion model.
//!
//! The host keeps rig's OpenAI wire and gives it an in-process HTTP client.
//! The client turns rig's encoded chat request into one Extism operation; the
//! guest forwards it through the host's broker and returns the provider's
//! bytes. SSE parsing, tool-call assembly and finish reasons stay in rig.
use super::super::providers::{ProviderInvocationContext, Running};
use crate::host::error::HostError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ene_plugin::{Event as PluginEvent, HttpChunk, HttpRequest, Operation as PluginOperation};
use ene_plugin_runtime::service::HttpBroker;
use http::header::HeaderName;
use rig_core::{
    catalog::{ModelFacts, ModelSpec},
    driver::DynModel,
    http_client::{
        BoxedStream, Bytes, DynHttpClient, Error as HttpError, HeaderMap, HeaderValue,
        HttpClientExt, LazyBody, MultipartForm, Request, Response, StatusCode, StreamingResponse,
    },
    operation::Completion,
    providers::{
        openai::{OpenAIConfig, Route, wire::Dialect},
        registry::ProviderConfig,
    },
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
///
/// The plugin serves a provider rig does not ship, so the model is connected
/// through the registry's declared-vendor path with a spec the host can hand
/// to the agent: the same facts that price and describe a catalog model. The
/// chat-completions route is forced because the ABI 7 generation contract is
/// that wire, not the Responses API an OpenAI config defaults to.
pub(crate) fn completion(
    provider: Arc<Running>,
    context: ProviderInvocationContext,
    model: String,
    image_input: bool,
) -> DynModel<Completion> {
    let endpoint = context.endpoint.clone();
    let provider_id = provider.provider_id();
    // An unlisted model is assumed to call tools; the host cannot prove
    // otherwise without a probe. Its limits stay unknown rather than guessed.
    let mut spec = ModelSpec::new(provider_id, model.clone()).with_tools(true);
    spec.input.image = image_input;
    // Match the wire descriptor's vendor name to `spec.provider` so
    // `ModelFacts::for_model` returns the bound spec even when a plugin
    // shadows a catalog-known vendor (where `ProviderId::declare` returns the
    // registered/catalog-only id rather than a declared id).
    let dialect = Dialect::gateway(provider_id.vendor(), "", "");
    let config = ProviderConfig::OpenAi(
        OpenAIConfig::with_key(&dialect, BROKER_KEY)
            .with_base_url(endpoint)
            .with_route(Route::Chat),
    );
    config.completion_model(
        &model,
        DynHttpClient::new(GuestClient { provider, context }),
        ModelFacts::new(spec),
    )
}

/// rig's HTTP transport, answered by one Extism invocation.
#[derive(Clone)]
struct GuestClient {
    provider: Arc<Running>,
    context: ProviderInvocationContext,
}

type Invocation = Pin<Box<dyn Future<Output = Result<serde_json::Value, HostError>> + Send>>;

struct Exchange {
    invocation: Invocation,
    events: mpsc::Receiver<PluginEvent>,
    provider: Arc<Running>,
    broker: Arc<HttpBroker>,
    handles: Vec<String>,
    status: Option<StatusCode>,
    headers: Option<HeaderMap>,
    pending: VecDeque<Bytes>,
    done: bool,
    invocation_done: bool,
    completed: bool,
    failed: bool,
}

// `rig_core::http_client::Error` (`HttpError`) is owned by an external crate and exceeds
// Clippy's 128-byte error threshold; remove if rig-core boxes its error variants.
#[allow(clippy::result_large_err)]
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
        let result = self.next_inner().await;
        match &result {
            Ok(None) => self.completed = true,
            Err(_) => self.failed = true,
            Ok(Some(_)) => {}
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<Bytes>, HttpError> {
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
        let Some(status) = self.status else {
            self.failed = true;
            return Err(protocol("the plugin sent no response line"));
        };
        Ok((status, self.headers.take().unwrap_or_default()))
    }
}

impl Drop for Exchange {
    /// Dropping the reply cancels the Extism call and closes any broker handle
    /// it opened. The operation is never retried by this path. Only a drop
    /// before a terminal reply or an error marks the provider for reload; a
    /// failed call keeps the old policy.
    fn drop(&mut self) {
        if !self.completed && !self.failed {
            self.provider.note_cancelled();
        }
        for handle in &self.handles {
            let _discarded = self.broker.close(handle);
        }
    }
}

impl GuestClient {
    // `HttpError` is defined by `rig-core` and exceeds 128 bytes; remove when `Exchange` no longer returns `HttpError`.
    #[allow(clippy::result_large_err)]
    fn exchange(&self, parts: &http::request::Parts, body: Bytes) -> Result<Exchange, HttpError> {
        let url = if parts.uri.scheme().is_some() {
            parts.uri.to_string()
        } else {
            let base = url::Url::parse(&self.context.endpoint)
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
        let services = self.context.services();
        let invocation: Invocation = Box::pin(async move {
            provider
                .invoke_streaming(PluginOperation::GenerateHttp { request }, services, sender)
                .await
        });
        Ok(Exchange {
            invocation,
            events,
            provider: Arc::clone(&self.provider),
            broker: Arc::clone(&self.context.broker),
            handles: Vec::new(),
            status: None,
            headers: None,
            pending: VecDeque::new(),
            done: false,
            invocation_done: false,
            completed: false,
            failed: false,
        })
    }
}

// `HttpClientExt` requires returning `'static` futures while extracting `&self` fields
// synchronously, so `async fn` cannot be used; remove if `HttpClientExt` adopts async fn.
#[allow(clippy::manual_async_fn, clippy::result_large_err)]
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
            if !status.is_success() {
                exchange.failed = true;
            }
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
        async {
            Err(HttpError::Instance(Box::new(std::io::Error::other(
                "chat completions does not use multipart",
            ))))
        }
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
            if !status.is_success() {
                exchange.failed = true;
            }
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
