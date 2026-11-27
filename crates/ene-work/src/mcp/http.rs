//! Bind MCP HTTP I/O to the lifetime of its connection.
use super::until_cancelled;
use futures_util::StreamExt as _;
use reqwest::{
    Client,
    header::{HeaderName, HeaderValue},
};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::{
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
            StreamableHttpResponseLimits,
        },
    },
};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

type Response<T> = Result<T, StreamableHttpError<reqwest::Error>>;

/// Keep rmcp's Reqwest client and size limits. Cancel work inside its HTTP
/// worker as well as waits made through the outer transport.
#[derive(Clone)]
pub(super) struct CancellableHttp {
    pub(super) client: Client,
    pub(super) stop: CancellationToken,
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
