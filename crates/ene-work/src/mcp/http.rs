//! Bind MCP HTTP I/O to the lifetime of its connection.
use super::until_cancelled;
use futures_util::StreamExt as _;
use reqwest::{
    Client,
    header::{HeaderName, HeaderValue},
};
use rmcp::{
    RoleClient,
    model::ClientJsonRpcMessage,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        Transport,
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransport,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
            StreamableHttpResponseLimits,
        },
    },
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

type Response<T> = Result<T, StreamableHttpError<reqwest::Error>>;

pub(super) struct HttpTransport(Option<StreamableHttpClientTransport<CancellableHttp>>);

impl HttpTransport {
    pub(super) fn new(
        client: Client,
        stop: CancellationToken,
        config: StreamableHttpClientTransportConfig,
    ) -> Self {
        Self(Some(StreamableHttpClientTransport::with_client(
            CancellableHttp { client, stop },
            config,
        )))
    }
}

impl Transport<RoleClient> for HttpTransport {
    type Error = StreamableHttpError<reqwest::Error>;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Response<()>> + Send + 'static {
        let send = self.0.as_mut().map(|transport| transport.send(item));
        async move {
            send.ok_or(StreamableHttpError::TransportChannelClosed)?
                .await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.0.as_mut()?.receive().await
    }

    async fn close(&mut self) -> Response<()> {
        let Some(mut transport) = self.0.take() else {
            return Ok(());
        };
        // rmcp normally spends up to five seconds on session cleanup. If its
        // worker instead blocks forwarding to the unread inbound channel,
        // drop that receiver after the deadline to unblock the worker as well.
        tokio::time::timeout(Duration::from_secs(6), transport.close())
            .await
            .unwrap_or(Err(StreamableHttpError::TransportChannelClosed))
    }
}

/// Keep rmcp's Reqwest client and size limits. Cancel work inside its HTTP
/// worker as well as waits made through the outer transport.
#[derive(Clone)]
struct CancellableHttp {
    client: Client,
    stop: CancellationToken,
}

impl StreamableHttpClient for CancellableHttp {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Response<StreamableHttpPostResponse> {
        self.post_message_with_response_limits(
            uri,
            message,
            session_id,
            auth,
            headers,
            Default::default(),
        )
        .await
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        limits: StreamableHttpResponseLimits,
    ) -> Response<StreamableHttpPostResponse> {
        let response = until_cancelled(
            &self.stop,
            self.client
                .post_message_with_response_limits(uri, message, session_id, auth, headers, limits),
        )
        .await
        .ok_or(StreamableHttpError::TransportChannelClosed)??;
        Ok(match response {
            StreamableHttpPostResponse::Sse(stream, session) => StreamableHttpPostResponse::Sse(
                stream
                    .take_until(self.stop.clone().cancelled_owned())
                    .boxed(),
                session,
            ),
            other => other,
        })
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Response<()> {
        // The rmcp worker bounds this protocol cleanup for an existing session.
        self.client
            .delete_session(uri, session_id, auth, headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Response<BoxedSseResponse> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth,
            headers,
            StreamableHttpResponseLimits::default().max_sse_event_size,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Response<BoxedSseResponse> {
        let stream = until_cancelled(
            &self.stop,
            self.client.get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth,
                headers,
                max_sse_event_size,
            ),
        )
        .await
        .ok_or(StreamableHttpError::TransportChannelClosed)??;
        Ok(stream
            .take_until(self.stop.clone().cancelled_owned())
            .boxed())
    }
}
