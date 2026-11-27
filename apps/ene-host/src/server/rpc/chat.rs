//! Chat service: message acceptance, history and turns.
use super::Services;
use ene_rpc as w;

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::ChatService for Services {
    async fn send_message(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::SendMessageRequest>,
    ) -> connectrpc::ServiceResult<w::Acceptance> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.accept(&request)).await
    }
    async fn stop(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::StopRequest>,
    ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
        let request = request.to_owned_message();
        self.call(move |kernel| {
            kernel.stop_turn(&request)?;
            Ok(ene_rpc::Empty::default())
        })
        .await
    }
    async fn get_history(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::HistoryRequest>,
    ) -> connectrpc::ServiceResult<w::History> {
        let request = request.to_owned_view();
        self.read(move |kernel| kernel.history(request.reborrow()))
            .await
    }
    async fn get_message(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::MessageRequest>,
    ) -> connectrpc::ServiceResult<w::Message> {
        let request = request.to_owned_view();
        self.read(move |kernel| kernel.message(request.reborrow()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Runtime, agent};
    use axum::body::Body;
    use connectrpc::{HasMessageView, StreamMessage};
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use rig_core::{completion::Message, conversation::Conversation as History};
    use rig_ecs::prelude::Conversation;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    /// Captures the history an in-process RPC read produced, without registering
    /// anything on the ECS world: the request path hands the value to this channel.
    static OBSERVED: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<w::History>>> =
        parking_lot::Mutex::new(None);

    fn observed_history(
        kernel: &mut crate::host::agent::AgentKernel<'_>,
        request: &w::HistoryRequestView<'_>,
    ) -> crate::host::error::Result<w::History> {
        let history = kernel.history(request)?;
        // The test installs the sender before issuing the call; a missing sender
        // means the test did not arm the probe, which is its own failure.
        let sender = OBSERVED
            .lock()
            .take()
            .expect("the test arms the observed channel before calling");
        sender
            .send(history.clone())
            .expect("test receiver is alive");
        Ok(history)
    }

    impl Services {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = "observed_history", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
        async fn observed_history(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, w::HistoryRequest>,
        ) -> connectrpc::ServiceResult<w::History> {
            let request = request.to_owned_view();
            self.read(move |kernel| observed_history(kernel, request.reborrow()))
                .await
        }
    }

    #[tokio::test]
    async fn retained_history_runs_after_request_body_and_reply_are_dropped() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let (observed, received) = tokio::sync::oneshot::channel();
        let companion = runtime
            .read(move |app| {
                let companion = crate::host::mind::create(
                    app.world_mut(),
                    w::CreateCompanionRequest {
                        name: "reader".into(),
                        ..Default::default()
                    },
                )?
                .id;
                let entity = agent::agent_of(app.world_mut(), &companion).unwrap();
                app.world_mut()
                    .entity_mut(entity)
                    .insert(Conversation(History::from_messages([Message::user(
                        "retained request reached the host",
                    )])));
                Ok(companion)
            })
            .await
            .unwrap();
        *OBSERVED.lock() = Some(observed);
        let (entered, blocked) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = runtime.read(move |_| {
            entered.send(()).unwrap();
            released
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        });
        blocked.await.unwrap();
        let service = Services::new(runtime.clone(), CancellationToken::new());
        {
            let message = StreamMessage::from_message(&w::HistoryRequest {
                companion_id: companion,
                limit: Some(1),
                ..Default::default()
            });
            let request = connectrpc::ServiceRequest::from_parts(message.view(), message.bytes());
            let mut reply = Box::pin(service.observed_history(
                connectrpc::RequestContext::new(http::HeaderMap::new()),
                request,
            ));
            assert!(futures_util::poll!(reply.as_mut()).is_pending());
        }
        release.send(()).unwrap();
        blocker.await.unwrap();
        let history = received.await.unwrap();
        assert_eq!(history.items.len(), 1);
        assert_eq!(history.items[0].text, "retained request reached the host");
        runtime.shutdown().await.unwrap();
    }

    async fn seed_reader_companion(runtime: &Runtime) -> String {
        runtime
            .read(|app| {
                let companion = crate::host::mind::create(
                    app.world_mut(),
                    w::CreateCompanionRequest {
                        name: "reader".into(),
                        ..Default::default()
                    },
                )?
                .id;
                let entity = agent::agent_of(app.world_mut(), &companion).unwrap();
                app.world_mut()
                    .entity_mut(entity)
                    .insert(Conversation(History::from_messages([
                        Message::user("first"),
                        Message::assistant("second"),
                        Message::user("third"),
                    ])));
                Ok(companion)
            })
            .await
            .unwrap()
    }

    async fn assert_history_and_message_format(
        router: &axum::Router,
        companion: &str,
        content_type: &str,
        history_body: Vec<u8>,
    ) {
        let response = router
            .clone()
            .oneshot(
                Request::post("/rpc/ene.host.v1.ChatService/GetHistory")
                    .header("host", "localhost")
                    .header("authorization", "Bearer owner")
                    .header("content-type", content_type)
                    .body(Body::from(history_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let items: Vec<(String, String)> = if content_type == "application/proto" {
            w::History::decode_view(&body)
                .unwrap()
                .items
                .iter()
                .map(|item| (item.id.to_owned(), item.text.to_owned()))
                .collect()
        } else {
            serde_json::from_slice::<w::History>(&body)
                .unwrap()
                .items
                .into_iter()
                .map(|item| (item.id, item.text))
                .collect()
        };
        assert_eq!(
            items
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["second", "third"]
        );
        let request = w::MessageRequest {
            companion_id: companion.to_owned(),
            message_id: items[0].0.clone(),
            ..Default::default()
        };
        let body = if content_type == "application/proto" {
            StreamMessage::from_message(&request).bytes().to_vec()
        } else {
            serde_json::to_vec(&request).unwrap()
        };
        let response = router
            .clone()
            .oneshot(
                Request::post("/rpc/ene.host.v1.ChatService/GetMessage")
                    .header("host", "localhost")
                    .header("authorization", "Bearer owner")
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = if content_type == "application/proto" {
            w::Message::decode_view(&body).unwrap().text.to_owned()
        } else {
            serde_json::from_slice::<w::Message>(&body).unwrap().text
        };
        assert_eq!(text, "second");
    }

    #[tokio::test]
    async fn history_requests_preserve_paging_and_errors_for_proto_and_json() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let companion = seed_reader_companion(&runtime).await;
        let router = super::super::router(
            runtime.clone(),
            crate::server::auth::Guard::with_allowed_hosts("owner".into(), ""),
            CancellationToken::new(),
        );
        let request = w::HistoryRequest {
            companion_id: companion.clone(),
            limit: Some(2),
            ..Default::default()
        };
        let mut proto = StreamMessage::from_message(&request).bytes().to_vec();
        proto.extend_from_slice(&[0xa0, 0x06, 0x07]);
        for (content_type, body) in [
            ("application/proto", proto),
            ("application/json", serde_json::to_vec(&request).unwrap()),
        ] {
            assert_history_and_message_format(&router, &companion, content_type, body).await;
        }
        for (path, body, code) in [
            (
                "GetHistory",
                serde_json::json!({"companionId":"missing"}).to_string(),
                "not_found",
            ),
            (
                "GetMessage",
                serde_json::json!({"companionId":companion,"messageId":"not-a-number"}).to_string(),
                "invalid_argument",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::post(format!("/rpc/ene.host.v1.ChatService/{path}"))
                        .header("host", "localhost")
                        .header("authorization", "Bearer owner")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["code"], code);
        }
        runtime.shutdown().await.unwrap();
    }
}
