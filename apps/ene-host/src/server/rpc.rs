//! Connect service implementations and the owner-authentication boundary.
use crate::host::{
    self, Runtime, chat, client, error::HostError, mind, model, settings, wire, work,
};
use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::Response as HttpResponse,
};
use connectrpc::{
    ConnectError, InboundStream, RequestContext, Response, Router, ServiceRequest, ServiceResult,
    ServiceStream,
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
        operation: impl FnOnce(&mut App) -> host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .call(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }

    async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .read(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }
}

impl From<HostError> for ConnectError {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Invalid(message) => ConnectError::invalid_argument(message),
            HostError::NotFound(message) => ConnectError::not_found(message),
            HostError::Conflict(message) => ConnectError::aborted(message),
            HostError::AlreadyExists(message) => ConnectError::already_exists(message),
            HostError::Permission(message) => ConnectError::permission_denied(message),
            HostError::Exhausted(message) => ConnectError::resource_exhausted(message),
            HostError::Unavailable(message) => ConnectError::unavailable(message),
            HostError::Storage(_) => {
                ConnectError::unavailable("state storage failed; restart after repairing storage")
            }
            HostError::Internal(message) => ConnectError::internal(message),
        }
    }
}

pub(super) fn router(runtime: Runtime, token: String, shutdown: CancellationToken) -> axum::Router {
    let services = Arc::new(Services { runtime, shutdown });
    let connect = w::HostServiceExt::register(services.clone(), Router::new());
    let connect = w::ChatServiceExt::register(services.clone(), connect);
    let connect = w::SettingsServiceExt::register(services.clone(), connect);
    let connect = w::WorkServiceExt::register(services.clone(), connect);
    let connect = w::ClientServiceExt::register(services.clone(), connect);
    let connect = w::VoiceServiceExt::register(services, connect);
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
    let host_ok = request
        .headers()
        .get(http::header::HOST)
        .and_then(|header| header.to_str().ok())
        .is_none_or(loopback_host);
    let origin_ok = request
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|header| header.to_str().ok())
        .is_none_or(loopback_origin);
    if !allowed || !host_ok || !origin_ok {
        return ConnectError::unauthenticated("owner credential required")
            .into_http_response(request.headers())
            .map(axum::body::Body::new);
    }
    next.run(request).await
}

fn loopback_host(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(host, tail)| {
            host == "::1" && (tail.is_empty() || tail.starts_with(':'))
        });
    }
    let host = value.split(':').next().unwrap_or(value);
    matches!(host, "localhost" | "127.0.0.1")
}

fn loopback_origin(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(host, tail)| {
            host == "::1" && (tail.is_empty() || tail.starts_with(':'))
        });
    }
    let host = host.split(':').next().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1")
}

#[allow(refining_impl_trait)]
impl w::HostService for Services {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Snapshot> {
        self.read(host::snapshot).await
    }

    async fn subscribe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeRequest>,
    ) -> ServiceResult<ServiceStream<w::Notification>> {
        let client_id = (!request.client_id.is_empty()).then(|| request.client_id.to_owned());
        let (snapshot, mut events) = self
            .runtime
            .read(move |app| host::subscribe(app, client_id.as_deref()))
            .await
            .map_err(ConnectError::from)?;
        let initial = w::Notification {
            cursor: snapshot.cursor.clone(),
            kind: "initial_state".into(),
            event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
            ..Default::default()
        };
        let target_cursor = snapshot_cursor(&initial);
        let stream = async_stream::stream! {
            yield Ok(initial);
            loop {
                match events.recv().await {
                    Ok(event) => yield Ok(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(w::Notification {
                            kind: "resync".into(),
                            event: Some(w::notification::Event::ResyncRequired(Box::new(
                                w::ResyncRequired {
                                    reason: "subscription lagged".into(),
                                    target_cursor: target_cursor.clone().into(),
                                    ..Default::default()
                                },
                            ))),
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
                items: mind::companions(app.world().resource::<host::db::Store>())?,
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
        self.read(move |app| {
            let store = app.world().resource::<host::db::Store>();
            Ok(wire::companion(&mind::companion(store, &id)?))
        })
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

    async fn delete_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        self.call(move |app| {
            mind::delete_companion(app.world_mut(), &id)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn get_mind(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::MindView> {
        let id = request.id.to_owned();
        self.read(move |app| mind::mind_view(app.world().resource::<host::db::Store>(), &id))
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

    async fn list_memories(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListMemoriesRequest>,
    ) -> ServiceResult<w::Memories> {
        let companion = request.companion_id.to_owned();
        let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
        self.read(move |app| {
            let store = app.world().resource::<host::db::Store>();
            let items = store
                .memories_for(&companion)?
                .into_iter()
                .take(limit)
                .map(|memory| wire::memory_item(&memory))
                .collect();
            Ok(w::Memories {
                items,
                ..Default::default()
            })
        })
        .await
    }
}

#[allow(refining_impl_trait)]
impl w::ChatService for Services {
    async fn send_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendMessageRequest>,
    ) -> ServiceResult<w::Acceptance> {
        let request = request.to_owned_message();
        self.call(move |app| chat::accept(app, request)).await
    }

    async fn stop(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StopRequest>,
    ) -> ServiceResult<w::Empty> {
        let companion = request.companion_id.to_owned();
        self.call(move |app| {
            chat::stop(app, &companion)?;
            Ok(w::Empty::default())
        })
        .await
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
        self.read(|app| settings::settings(app.world().resource::<host::db::Store>()))
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
            .await
            .map_err(ConnectError::from)?;
        for reference in forgotten {
            let _ = self.runtime.credentials().forget(&reference).await;
        }
        Response::ok(updated)
    }

    async fn list_pending_approvals(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PendingApprovals> {
        self.read(|app| settings::pending_approvals(app.world().resource::<host::db::Store>()))
            .await
    }

    async fn submit_approval(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubmitApprovalRequest>,
    ) -> ServiceResult<w::SubmitApprovalResponse> {
        let request = request.to_owned_message();
        self.call(move |app| settings::submit_approval(app.world_mut(), request))
            .await
    }

    async fn list_policy_rules(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyRules> {
        self.read(|app| settings::policy_rules(app.world().resource::<host::db::Store>()))
            .await
    }

    async fn update_policy_rules(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyRulesRequest>,
    ) -> ServiceResult<w::PolicyRules> {
        let request = request.to_owned_message();
        self.call(move |app| settings::update_policy_rules(app.world_mut(), request))
            .await
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

    async fn list_models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        let id = request.id.to_owned();
        let connection = connection(&self.runtime, &id).await?;
        let secret = model::secret(&self.runtime.credentials(), &connection)
            .ok_or_else(|| ConnectError::permission_denied("credential_missing"))?;
        let (items, _) = model::probe(&connection, &secret)
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::Models {
            items,
            ..Default::default()
        })
    }

    async fn register_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let id = request.id.to_owned();
        let secret = request.secret.filter(|secret| !secret.is_empty());
        connection(&self.runtime, &id).await?;
        let secret = secret.ok_or_else(|| ConnectError::invalid_argument("secret is required"))?;
        let reference = format!("connection:{id}");
        self.runtime
            .credentials()
            .save(&reference, secret)
            .await
            .map_err(ConnectError::from)?;
        let saved = self
            .runtime
            .call({
                let id = id.clone();
                let reference = reference.clone();
                move |app| {
                    let store = app.world().resource::<host::db::Store>();
                    let mut connection = store
                        .connection(&id)?
                        .ok_or_else(|| HostError::not_found("connection not found"))?;
                    connection.credential_ref = Some(reference);
                    connection.credential_state = host::settings::CredentialState::Registered;
                    store.save_connection(&connection)
                }
            })
            .await;
        if saved.is_err() {
            let _ = self
                .runtime
                .credentials()
                .forget(&format!("connection:{id}"))
                .await;
        }
        saved.map_err(ConnectError::from)?;
        self.runtime
            .call({
                let id = id.clone();
                move |app| host::settings::credential_changed(app.world_mut(), &id)
            })
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::CredentialResult {
            credential_state: "registered".into(),
            ..Default::default()
        })
    }

    async fn revoke_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        let reference = format!("connection:{id}");
        let cleanup = self.runtime.credentials().forget(&reference).await;
        let save_id = id.clone();
        self.runtime
            .call(move |app| {
                let store = app.world().resource::<host::db::Store>();
                let mut connection = store
                    .connection(&save_id)?
                    .ok_or_else(|| HostError::not_found("connection not found"))?;
                connection.credential_ref = None;
                connection.credential_state = host::settings::CredentialState::Missing;
                store.save_connection(&connection)
            })
            .await
            .map_err(ConnectError::from)?;
        self.runtime
            .call({
                let id = id.clone();
                move |app| host::settings::credential_changed(app.world_mut(), &id)
            })
            .await
            .map_err(ConnectError::from)?;
        cleanup.map_err(ConnectError::from)?;
        Response::ok(w::Empty::default())
    }

    async fn check_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        let id = request.id.to_owned();
        let request_key = request.request_key.to_owned();
        let connection = connection(&self.runtime, &id).await?;
        let secret = model::secret(&self.runtime.credentials(), &connection)
            .ok_or_else(|| ConnectError::permission_denied("credential_missing"))?;
        let (models, capabilities) = model::probe(&connection, &secret)
            .await
            .map_err(ConnectError::from)?;
        let model_id = models
            .first()
            .map_or_else(String::new, |model| model.id.clone());
        let capabilities_text = wire::json(&capabilities);
        self.runtime
            .call({
                let id = id.clone();
                let capabilities = capabilities.clone();
                move |app| {
                    let store = app.world().resource::<host::db::Store>();
                    let mut connection = store
                        .connection(&id)?
                        .ok_or_else(|| HostError::not_found("connection not found"))?;
                    connection.last_check = capabilities;
                    store.save_connection(&connection)
                }
            })
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::CheckResult {
            request_id: request_key,
            capabilities: capabilities_text,
            success: true,
            model_id,
            ..Default::default()
        })
    }
}

#[allow(refining_impl_trait)]
impl w::WorkService for Services {
    async fn list_works(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListWorksRequest>,
    ) -> ServiceResult<w::WorkViews> {
        let companion = request.companion_id.to_owned();
        self.read(move |app| {
            Ok(w::WorkViews {
                items: work::list(app.world_mut(), &companion)?,
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
        self.read(move |app| work::view(app.world_mut(), &id)).await
    }

    async fn start_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StartWorkRequest>,
    ) -> ServiceResult<w::WorkSession> {
        let request = request.to_owned_message();
        self.call(move |app| {
            work::start(
                app,
                request.companion_id,
                request.title,
                request.instruction,
                request.client_id,
            )
        })
        .await
    }

    async fn send_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendWorkRequest>,
    ) -> ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| work::send(app, request)).await
    }

    async fn answer_inquiry(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::AnswerInquiryRequest>,
    ) -> ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| work::answer_inquiry(app, request))
            .await
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
        request: ServiceRequest<'_, w::WorkRequest>,
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
                items: client::list(
                    app.world().resource::<host::db::Store>(),
                    app.world().resource::<host::resources::DeviceStreams>(),
                )?,
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

    async fn report_perception(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::PerceptionEvent>,
    ) -> ServiceResult<w::Empty> {
        let event = request.to_owned_message();
        self.call(move |app| {
            client::report_perception(app.world_mut(), event)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn device_stream(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        let client_id = request.id.to_owned();
        let subscription = self
            .runtime
            .call(move |app| client::subscribe(app.world_mut(), &client_id))
            .await
            .map_err(ConnectError::from)?;
        let (id, mut receiver, pending) =
            (subscription.id, subscription.receiver, subscription.pending);
        let stream = async_stream::stream! {
            yield Ok(w::DeviceEvent {
                event: Some(w::device_event::Event::Ready(Box::new(w::DeviceReady {
                    subscription_id: id,
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
        self.call(move |app| client::complete(app.world_mut(), request))
            .await
    }

    async fn get_operation(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::DeviceOperationState> {
        let id = request.id.to_owned();
        self.read(move |app| client::get_operation(app.world_mut(), &id))
            .await
    }
}

#[allow(refining_impl_trait)]
impl w::VoiceService for Services {
    async fn stream_audio_input(
        &self,
        _: RequestContext,
        _: InboundStream<w::AudioInputChunk>,
    ) -> ServiceResult<ServiceStream<w::TranscriptionEvent>> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }

    async fn subscribe_audio_output(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::SubscribeAudioRequest>,
    ) -> ServiceResult<ServiceStream<w::AudioOutputEvent>> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }

    async fn report_playback_status(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::PlaybackStatusReport>,
    ) -> ServiceResult<w::ReportPlaybackStatusResponse> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }
}

fn snapshot_cursor(notification: &w::Notification) -> w::Cursor {
    match notification.event.as_ref() {
        Some(w::notification::Event::InitialState(snapshot)) => {
            snapshot.cursor.as_option().cloned().unwrap_or_default()
        }
        _ => w::Cursor::default(),
    }
}

async fn connection(
    runtime: &Runtime,
    id: &str,
) -> Result<host::settings::Connection, ConnectError> {
    let id = id.to_owned();
    runtime
        .call(move |app| {
            app.world()
                .resource::<host::db::Store>()
                .connection(&id)?
                .ok_or_else(|| HostError::not_found("connection not found"))
        })
        .await
        .map_err(ConnectError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_authorities_and_origins_are_trusted() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:1234",
            "localhost",
            "localhost:1234",
            "[::1]",
            "[::1]:1234",
        ] {
            assert!(loopback_host(host), "{host}");
        }
        for host in [
            "example.com",
            "example.com:80",
            "0.0.0.0:80",
            "[2001:db8::1]",
        ] {
            assert!(!loopback_host(host), "{host}");
        }
        for origin in [
            "http://127.0.0.1:1234",
            "http://localhost:1234",
            "https://[::1]:1234",
        ] {
            assert!(loopback_origin(origin), "{origin}");
        }
        for origin in ["https://example.com", "null", "http://0.0.0.0:80"] {
            assert!(!loopback_origin(origin), "{origin}");
        }
    }
}
