//! Connect service implementations and their shared owner-authentication boundary.
use crate::host::{self, Host, Runtime, chat, client, mind, model, settings, work};
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response as HttpResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;
use rig_ecs::prelude::App;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Services {
    runtime: Runtime,
    shutdown: CancellationToken,
}

impl Services {
    async fn call<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut App) -> Result<T, ConnectError> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(self.runtime.call(op).await?)
    }

    /// A read: the same serialization, without advancing a frame.
    async fn read<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut App) -> Result<T, ConnectError> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(self.runtime.read(op).await?)
    }
}

pub(super) fn router(runtime: Runtime, token: String, shutdown: CancellationToken) -> axum::Router {
    let services = Arc::new(Services { runtime, shutdown });
    let connect = w::HostServiceExt::register(services.clone(), Router::new());
    let connect = w::ChatServiceExt::register(services.clone(), connect);
    let connect = w::SettingsServiceExt::register(services.clone(), connect);
    let connect = w::WorkServiceExt::register(services.clone(), connect);
    let connect = w::ClientServiceExt::register(services, connect);
    axum::Router::new()
        .nest_service("/rpc", connect.into_axum_router())
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
        return ConnectError::unauthenticated("owner credential required")
            .into_http_response(request.headers())
            .map(axum::body::Body::new);
    }
    next.run(request).await
}

/// Whether the subscription of `client` gets `notification`: one addressed
/// to a client is that client's alone, and an anonymous subscription gets
/// every broadcast.
fn for_subscription(notification: &w::Notification, client: Option<&str>) -> bool {
    match (client, notification.client_id.as_deref()) {
        (Some(client), Some(target)) => client == target,
        _ => true,
    }
}

fn require_target(target: Option<&w::TargetView<'_>>) -> Result<w::Target, ConnectError> {
    let view = target.ok_or_else(|| ConnectError::invalid_argument("target is required"))?;
    Ok(w::Target {
        companion_id: view.companion_id.to_owned(),
        work_id: view.work_id.to_owned(),
        ..Default::default()
    })
}

#[allow(refining_impl_trait)]
impl w::HostService for Services {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SnapshotRequest>,
    ) -> ServiceResult<w::Snapshot> {
        let subscription = request.subscription_id.map(str::to_owned);
        self.read(move |app| host::snapshot(app, subscription))
            .await
    }

    async fn subscribe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeRequest>,
    ) -> ServiceResult<ServiceStream<w::HostEvent>> {
        let client_id = request
            .client_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let (snapshot, mut events, subscription) = self
            .runtime
            .read({
                let client_id = client_id.clone();
                move |app| host::subscribe(app, client_id.as_deref())
            })
            .await?;
        let initial = w::InitialState {
            subscription_id: subscription,
            snapshot: snapshot.into(),
            ..Default::default()
        };
        let stream = async_stream::stream! {
            yield Ok(w::HostEvent {
                event: Some(w::host_event::Event::Initial(Box::new(initial))),
                ..Default::default()
            });
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if !for_subscription(&event, client_id.as_deref()) {
                            continue;
                        }
                        yield Ok(w::HostEvent {
                            event: Some(w::host_event::Event::Notification(Box::new(event))),
                            ..Default::default()
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(w::HostEvent {
                            event: Some(w::host_event::Event::ResyncRequired(Box::default())),
                            ..Default::default()
                        });
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn list_companions(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Companions> {
        self.read(|app| {
            Ok(w::Companions {
                items: mind::companions(&app.world().resource::<Host>().db)?,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Companion> {
        let id = request.id.to_owned();
        self.read(
            move |app| Ok(mind::companion(&app.world().resource::<Host>().db, &id)?.companion),
        )
        .await
    }

    async fn create_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CreateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |app| mind::create_companion(app.world_mut(), request))
            .await
    }

    async fn update_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |app| mind::update_companion(app.world_mut(), request))
            .await
    }

    async fn get_mind(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::MindView> {
        let id = request.id.to_owned();
        self.read(move |app| mind::mind_view(&app.world().resource::<Host>().db, &id))
            .await
    }

    async fn update_mind_profile(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateMindProfileRequest>,
    ) -> ServiceResult<w::MindView> {
        let request = request.to_owned_message();
        self.call(move |app| mind::update_mind_profile(app.world_mut(), request))
            .await
    }

    async fn control_memory(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::MemoryControlRequest>,
    ) -> ServiceResult<w::MemoryControlResult> {
        let request = request.to_owned_message();
        self.call(move |app| mind::control_memory(app.world_mut(), request))
            .await
    }
}

#[allow(refining_impl_trait)]
impl w::ChatService for Services {
    async fn send_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendMessageRequest>,
    ) -> ServiceResult<w::MessageAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| chat::accept(app, request)).await
    }

    async fn stop(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StopRequest>,
    ) -> ServiceResult<w::Empty> {
        let target = require_target(request.target.as_option())?;
        self.call(move |app| chat::stop(app, target)).await
    }

    async fn get_history(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::HistoryRequest>,
    ) -> ServiceResult<w::History> {
        let request = request.to_owned_message();
        self.read(move |app| chat::history(app.world_mut(), request))
            .await
    }

    async fn get_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::MessageRequest>,
    ) -> ServiceResult<w::Message> {
        let request = request.to_owned_message();
        self.read(move |app| chat::message(app.world_mut(), request))
            .await
    }
}

#[allow(refining_impl_trait)]
impl w::SettingsService for Services {
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        self.read(|app| settings::settings(&app.world().resource::<Host>().db))
            .await
    }

    async fn update_settings(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        let request = request.to_owned_message();
        let (updated, forgotten) = self
            .runtime
            .call(move |app| settings::update_settings(app.world_mut(), request))
            .await?;
        for reference in forgotten {
            let _ = self.runtime.credentials().forget(&reference).await;
        }
        Response::ok(updated)
    }

    async fn list_grants(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::GrantsRequest>,
    ) -> ServiceResult<w::Grants> {
        let companion = request.companion_id.map(str::to_owned);
        self.read(move |app| {
            Ok(w::Grants {
                items: settings::grants(&app.world().resource::<Host>().db, companion.as_deref())?,
                ..Default::default()
            })
        })
        .await
    }

    async fn create_grant(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CreateGrantRequest>,
    ) -> ServiceResult<w::GrantResult> {
        let request = request.to_owned_message();
        self.call(move |app| {
            Ok(w::GrantResult {
                grant: settings::create_grant(app.world_mut(), request)?.into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn update_grant(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateGrantRequest>,
    ) -> ServiceResult<w::GrantResult> {
        let request = request.to_owned_message();
        self.call(move |app| {
            Ok(w::GrantResult {
                grant: settings::update_grant(app.world_mut(), request)?.into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn revoke_grant(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        self.call(move |app| {
            settings::revoke_grant(app.world_mut(), &id)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn register_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let id = request.id.to_owned();
        let secret = request.secret.map(str::to_owned);
        Response::ok(settings::register_credentials(&self.runtime, id, secret).await?)
    }

    async fn revoke_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        Response::ok(settings::revoke_credentials(&self.runtime, request.id.to_owned()).await?)
    }

    async fn check_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        Response::ok(
            settings::check_connection(
                &self.runtime,
                request.id.to_owned(),
                request.request_key.to_owned(),
            )
            .await?,
        )
    }

    async fn list_models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        Response::ok(settings::list_models(&self.runtime, request.id.to_owned()).await?)
    }

    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Providers> {
        Response::ok(w::Providers {
            items: model::descriptors(),
            ..Default::default()
        })
    }

    async fn get_availability(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::AvailabilityRequest>,
    ) -> ServiceResult<w::Availability> {
        let companion_id = request.companion_id.to_owned();
        let connection_id = request.connection_id.to_owned();
        self.read(move |app| {
            settings::availability(
                &app.world().resource::<Host>().db,
                &companion_id,
                &connection_id,
            )
        })
        .await
    }
}

#[allow(refining_impl_trait)]
impl w::WorkService for Services {
    async fn list_works(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListWorksRequest>,
    ) -> ServiceResult<w::WorkSessionList> {
        let companion = request.companion_id.to_owned();
        self.read(move |app| {
            Ok(w::WorkSessionList {
                items: work::list(app.world().resource::<Host>(), &companion)?,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::WorkRequest>,
    ) -> ServiceResult<w::WorkView> {
        let id = request.work_id.to_owned();
        self.read(move |app| work::view(app, &id)).await
    }

    async fn send_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendWorkRequest>,
    ) -> ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| work::send(app, request)).await
    }

    async fn stop_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StopWorkRequest>,
    ) -> ServiceResult<w::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |app| {
            work::stop(app, &id)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn close_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CloseWorkRequest>,
    ) -> ServiceResult<w::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |app| {
            work::close(app, &id)?;
            Ok(w::Empty::default())
        })
        .await
    }
}

#[allow(refining_impl_trait)]
impl w::ClientService for Services {
    async fn register_client(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::RegisterClientRequest>,
    ) -> ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |app| client::register(app, request)).await
    }

    async fn list(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Clients> {
        self.read(|app| {
            Ok(w::Clients {
                items: client::list(app.world().resource::<Host>())?,
                ..Default::default()
            })
        })
        .await
    }

    async fn activate(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ActivationRequest>,
    ) -> ServiceResult<w::Activation> {
        let request = request.to_owned_message();
        self.call(move |app| client::activate(app, request)).await
    }

    async fn set_capabilities(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SetCapabilitiesRequest>,
    ) -> ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |app| client::set_capabilities(app, request))
            .await
    }

    async fn device_stream(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        let client_id = request.id.to_owned();
        let (subscription, (mut receiver, pending)) = self
            .runtime
            .call(move |app| {
                let sub = client::subscribe(app, &client_id)?;
                Ok((sub.id, (sub.receiver, sub.pending)))
            })
            .await?;
        let stream = async_stream::stream! {
            yield Ok(w::DeviceEvent {
                event: Some(w::device_event::Event::Ready(Box::new(w::DeviceReady {
                    subscription_id: subscription,
                    ..Default::default()
                }))),
                ..Default::default()
            });
            for operation in pending {
                yield Ok(w::DeviceEvent {
                    event: Some(w::device_event::Event::Operation(Box::new(operation))),
                    ..Default::default()
                });
            }
            while let Some(operation) = receiver.recv().await {
                yield Ok(w::DeviceEvent {
                    event: Some(w::device_event::Event::Operation(Box::new(operation))),
                    ..Default::default()
                });
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn complete_operation(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::DeviceOperationResult>,
    ) -> ServiceResult<w::DeviceOperationState> {
        let request = request.to_owned_message();
        self.call(move |app| client::complete(app, request)).await
    }

    async fn get_operation(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::DeviceOperationState> {
        let id = request.id.to_owned();
        self.read(move |app| client::get_operation(app, &id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode, header};
    use tower::ServiceExt;

    #[test]
    fn a_notification_for_one_client_is_not_sent_to_another() {
        let notification = w::Notification {
            client_id: Some("client-a".into()),
            ..Default::default()
        };
        assert!(for_subscription(&notification, Some("client-a")));
        assert!(!for_subscription(&notification, Some("client-b")));
        assert!(for_subscription(&notification, None));
        let broadcast = w::Notification::default();
        assert!(for_subscription(&broadcast, Some("client-b")));
    }

    #[tokio::test]
    async fn owner_auth_guards_every_connect_route() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(temp.path()).await.unwrap();
        let app = router(
            runtime.clone(),
            "owner-token".into(),
            CancellationToken::new(),
        );
        for token in [None, Some("Bearer wrong")] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/rpc/ene.host.v1.HostService/GetSnapshot")
                .header(header::CONTENT_TYPE, "application/proto")
                .header("connect-protocol-version", "1");
            if let Some(token) = token {
                request = request.header(header::AUTHORIZATION, token);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rpc/ene.host.v1.HostService/GetSnapshot")
                    .header(header::CONTENT_TYPE, "application/proto")
                    .header("connect-protocol-version", "1")
                    .header(header::AUTHORIZATION, "Bearer owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        runtime.shutdown().await.unwrap();
    }
}
