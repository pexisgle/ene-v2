//! Connect service implementations and their shared owner authentication boundary.
use crate::{
    host::{self, Runtime},
    proto::{chat::v1::*, settings::v1::*},
};
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response as HttpResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router, ServiceRequest, ServiceResult, ServiceStream,
};
use futures_util::StreamExt;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Services {
    runtime: Runtime,
    shutdown: CancellationToken,
}

pub(super) fn router(runtime: Runtime, token: String, shutdown: CancellationToken) -> axum::Router {
    let services = Arc::new(Services { runtime, shutdown });
    let connect = ChatServiceExt::register(services.clone(), Router::new());
    SettingsServiceExt::register(services, connect)
        .into_axum_router()
        .layer(middleware::from_fn_with_state(token, authenticate))
}

async fn authenticate(State(token): State<String>, request: Request, next: Next) -> HttpResponse {
    let allowed = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(token.as_bytes())));
    if !allowed {
        return connectrpc::ConnectError::unauthenticated("owner credential required")
            .into_http_response(request.headers())
            .map(axum::body::Body::new);
    }
    next.run(request).await
}

fn require_target(target: Option<&TargetView<'_>>) -> Result<Target, ConnectError> {
    let view = target.ok_or_else(|| ConnectError::invalid_argument("target is required"))?;
    Ok(Target {
        companion_id: view.companion_id.to_owned(),
        work_id: view.work_id.to_owned(),
        ..Default::default()
    })
}

#[allow(refining_impl_trait)]
impl ChatService for Services {
    async fn send_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, SendMessageRequest>,
    ) -> ServiceResult<Acceptance> {
        let request = request.to_owned_message();
        Response::ok(
            self.runtime
                .call(move |app| host::accept(app, request))
                .await?,
        )
    }
    async fn stop(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, StopRequest>,
    ) -> ServiceResult<Empty> {
        let target = require_target(request.target.as_option())?;
        Response::ok(
            self.runtime
                .call(move |app| host::stop(app, target))
                .await?,
        )
    }
    async fn get_history(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, ReadRequest>,
    ) -> ServiceResult<History> {
        let target = require_target(request.target.as_option())?;
        Response::ok(
            self.runtime
                .call(move |app| {
                    let entity = host::target_entity(app.world_mut(), &target)?;
                    Ok(host::history(app.world(), entity))
                })
                .await?,
        )
    }
    async fn get_snapshot(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, ReadRequest>,
    ) -> ServiceResult<Snapshot> {
        let target = require_target(request.target.as_option())?;
        Response::ok(
            self.runtime
                .call(move |app| host::snapshot(app, &target))
                .await?,
        )
    }
    async fn subscribe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, ReadRequest>,
    ) -> ServiceResult<ServiceStream<Event>> {
        let target = require_target(request.target.as_option())?;
        let companion = target.companion_id.clone();
        let (snapshot, mut events) = self
            .runtime
            .call(move |app| host::subscribe(app, target))
            .await?;
        let updates = async_stream::try_stream! {
            yield Event { epoch: snapshot.epoch.clone(), sequence: snapshot.sequence, companion_id: companion.clone(),
                conversation_id: snapshot.conversation_id.clone(), kind: Some(event::Kind::Snapshot(Box::new(snapshot))), ..Default::default() };
            loop {
                let event = events.recv().await.map_err(|error| match error {
                    broadcast::error::RecvError::Lagged(_) => ConnectError::resource_exhausted("subscription lagged; reconnect for a fresh snapshot"),
                    broadcast::error::RecvError::Closed => ConnectError::unavailable("host storage or execution stopped"),
                })?;
                if event.companion_id == companion {
                    yield event;
                }
            }
        };
        Response::stream_ok(updates.take_until(self.shutdown.clone().cancelled_owned()))
    }
    async fn respond(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, RespondRequest>,
    ) -> ServiceResult<Empty> {
        let request = request.to_owned_message();
        Response::ok(
            self.runtime
                .call(move |app| host::respond(app, request))
                .await?,
        )
    }
}

#[allow(refining_impl_trait)]
impl SettingsService for Services {
    async fn get_settings(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, GetSettingsRequest>,
    ) -> ServiceResult<Settings> {
        let companion = request.companion_id.to_owned();
        Response::ok(
            self.runtime
                .call(move |app| host::settings(app.world(), &companion))
                .await?,
        )
    }
    async fn set_settings(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, SetSettingsRequest>,
    ) -> ServiceResult<Settings> {
        let request = request.to_owned_message();
        let settings = request
            .settings
            .into_option()
            .ok_or_else(|| ConnectError::invalid_argument("settings are required"))?;
        Response::ok(
            self.runtime
                .call(move |app| {
                    host::set_settings(
                        app,
                        request.companion_id,
                        request.expected_revision,
                        settings,
                    )
                })
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{chat::v1 as w, settings::v1::Settings};
    use buffa::{HasMessageView, Message, view::MessageView};
    use rig_core::{
        catalog::Catalog,
        operation::Completion,
        serve::{ErasedHandler, adapters::ModelAdapter},
        test_utils::{MockCompletionModel, MockStreamEvent},
    };
    use rig_ecs::{effects::Handler, model::Connection};
    use std::{sync::atomic::AtomicBool, time::Duration};
    use tokio::time::timeout;
    use tower::ServiceExt;

    #[tokio::test]
    async fn subscriptions_keep_lag_failure_and_shutdown_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(
            temp.path(),
            Arc::new(|_, _, _| panic!("reading subscriptions must not invoke a model")),
        )
        .unwrap();
        let shutdown = CancellationToken::new();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: shutdown.clone(),
        };
        let read = ReadRequest {
            target: Target {
                companion_id: "a".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let bytes = bytes::Bytes::from(read.encode_to_vec());
        let view = <ReadRequest as HasMessageView>::View::decode_view(&bytes).unwrap();
        let subscribe = || {
            service.subscribe(
                RequestContext::new(http::HeaderMap::new()),
                ServiceRequest::<ReadRequest>::from_parts(&view, &bytes),
            )
        };
        let mut lagged = subscribe().await.unwrap().body;
        assert!(matches!(
            lagged.next().await.unwrap().unwrap().kind,
            Some(event::Kind::Snapshot(_))
        ));
        let mut cancelled = subscribe().await.unwrap().body;
        for _ in 0..70 {
            runtime
                .call(|app| {
                    let settings = host::settings(app.world(), "a")?;
                    host::set_settings(app, "a".into(), settings.revision, settings)
                })
                .await
                .unwrap();
        }
        assert_eq!(
            lagged.next().await.unwrap().unwrap_err().code,
            connectrpc::ErrorCode::ResourceExhausted
        );
        assert!(lagged.next().await.is_none());
        let mut failed = subscribe().await.unwrap().body;
        assert!(matches!(
            failed.next().await.unwrap().unwrap().kind,
            Some(event::Kind::Snapshot(_))
        ));
        runtime.shutdown().await.unwrap();
        assert_eq!(
            failed.next().await.unwrap().unwrap_err().code,
            connectrpc::ErrorCode::Unavailable
        );
        assert!(failed.next().await.is_none());
        // Cancellation wins even when a snapshot and lag error are ready to emit.
        shutdown.cancel();
        assert!(cancelled.next().await.is_none());
    }

    #[tokio::test]
    async fn wire_clients_are_authenticated_and_connecting_never_changes_settings() {
        let temp = tempfile::tempdir().unwrap();
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("wire answer"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let factory = Arc::new(move |config: &Settings, _: bool, _: Arc<AtomicBool>| {
            let spec = Catalog::builtin()
                .resolve(&format!("openai/{}", config.companion_model))
                .unwrap()
                .shared();
            Ok(Connection {
                handler: Handler(ErasedHandler::new(ModelAdapter::<Completion>::new(
                    spec.reference(),
                    model.clone(),
                ))),
                spec,
            })
        });
        let runtime = Runtime::start(temp.path(), factory).unwrap();
        let shutdown = CancellationToken::new();
        let app = router(runtime.clone(), "owner-token".into(), shutdown.clone());
        let response = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/chat.v1.ChatService/GetSnapshot")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (end, ended) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = ended.await;
                })
                .await
                .unwrap();
        });
        let target = w::Target {
            companion_id: "a".into(),
            ..Default::default()
        };
        let read = w::ReadRequest {
            target: target.clone().into(),
            ..Default::default()
        };
        let get_settings = GetSettingsRequest {
            companion_id: "a".into(),
            ..Default::default()
        };
        let first = crate::client::open(&endpoint, "owner-token").await.unwrap();
        let mut config = first
            .settings
            .get_settings(get_settings.clone())
            .await
            .unwrap()
            .into_owned();
        config.model_allowed = true;
        config.tools_allowed = true;
        first
            .settings
            .set_settings(SetSettingsRequest {
                companion_id: "a".into(),
                expected_revision: config.revision,
                settings: config.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let second = crate::client::open(&endpoint, "owner-token").await.unwrap();
        assert_eq!(
            first
                .settings
                .get_settings(get_settings.clone())
                .await
                .unwrap()
                .view()
                .revision,
            1
        );
        assert_eq!(
            second
                .settings
                .get_settings(get_settings)
                .await
                .unwrap()
                .view()
                .revision,
            1
        );
        let mut events = second.chat.subscribe(read.clone()).await.unwrap();
        assert!(matches!(
            events
                .message::<w::Event>()
                .await
                .unwrap()
                .unwrap()
                .to_owned_message()
                .kind,
            Some(w::event::Kind::Snapshot(_))
        ));
        let message = w::SendMessageRequest {
            target: target.clone().into(),
            request_key: "wire-key".into(),
            text: "wire input".into(),
            ..Default::default()
        };
        let accepted = first
            .chat
            .send_message(message.clone())
            .await
            .unwrap()
            .into_owned();
        timeout(Duration::from_secs(8), async {
            loop {
                if second
                    .chat
                    .get_history(read.clone())
                    .await
                    .unwrap()
                    .into_owned()
                    .messages
                    .iter()
                    .any(|m| m.text == "wire answer")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            second
                .chat
                .send_message(message)
                .await
                .unwrap()
                .view()
                .input_id,
            accepted.input_id
        );
        assert!(
            second
                .chat
                .get_history(read)
                .await
                .unwrap()
                .into_owned()
                .messages
                .iter()
                .any(|m| m.text == "wire answer")
        );
        first
            .chat
            .stop(w::StopRequest {
                target: target.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        shutdown.cancel();
        timeout(Duration::from_secs(3), async {
            while events.message::<w::Event>().await.unwrap().is_some() {}
        })
        .await
        .unwrap();
        drop(events);
        drop(first);
        drop(second);
        end.send(()).unwrap();
        timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        runtime.shutdown().await.unwrap();
    }
}
