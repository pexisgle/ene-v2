//! Authenticated generated services use the shared RPC messages directly.
use super::{error::ApiError, state::App};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use std::sync::Arc;

pub(super) fn service(app: App) -> connectrpc::ConnectRpcService {
    connectrpc::Router::new()
        .add_service(Arc::new(HostRpc { app: app.clone() }))
        .add_service(Arc::new(super::plugin_rpc::PluginRpc { app: app.clone() }))
        .add_service(Arc::new(super::device_rpc::DeviceRpc { app }))
        .into_axum_service()
}
struct HostRpc {
    app: App,
}
pub(super) fn failure(error: crate::domain::Error) -> ConnectError {
    ApiError::from(error).into_rpc()
}
pub(super) fn invalid() -> ConnectError {
    ApiError::invalid().into_rpc()
}
fn key(value: String) -> Result<String, ConnectError> {
    if value.is_empty() || value.len() > 200 || value.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(invalid());
    }
    Ok(value)
}
fn receipt<T: serde::de::DeserializeOwned>(
    answer: crate::ports::Answer<T>,
) -> Result<T, ConnectError> {
    use crate::ports::Outcome;
    let (outcome, value) = match answer {
        crate::ports::Answer::Success(value) => return Ok(value),
        crate::ports::Answer::Stored(outcome, value) => (outcome, value),
    };
    if matches!(outcome, Outcome::Pending | Outcome::Unavailable) {
        let mut body: w::ApiFailure = serde_json::from_value(value)
            .map_err(|_| ConnectError::internal("応答を変換できません"))?;
        body.http_status = if matches!(outcome, Outcome::Pending) {
            409
        } else {
            503
        };
        return Err(super::error::ApiError::from_failure(body).into_rpc());
    }
    serde_json::from_value::<T>(value).map_err(|_| ConnectError::internal("応答を読み取れません"))
}

#[allow(refining_impl_trait)]
impl w::HostService for HostRpc {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::SnapshotRequest>,
    ) -> ServiceResult<w::Snapshot> {
        Response::ok(
            self.app
                .snapshot_document(req.subscription_id)
                .await
                .map_err(ApiError::into_rpc)?,
        )
    }
    async fn subscribe(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<ServiceStream<w::HostEvent>> {
        let mut subscription = self.app.application.subscribe().await;
        let snapshot = self
            .app
            .snapshot_document(Some(&subscription.id))
            .await
            .map_err(ApiError::into_rpc)?;
        let cursor = snapshot.cursor.clone();
        let subscription_id = subscription.id.clone();
        let application = self.app.application.clone();
        let initial = w::InitialState {
            subscription_id: subscription.id,
            snapshot: snapshot.into(),
            ..Default::default()
        };
        let mut shutdown = self.app.application.shutdown();
        Response::stream_ok(async_stream::stream! {
            let _lease = subscription.lease;
            yield Ok(w::HostEvent { event: Some(w::host_event::Event::Initial(Box::new(initial))), ..Default::default() });
            loop {
                if *shutdown.borrow() { break; }
                tokio::select! {
                    _ = shutdown.changed() => break,
                    received = subscription.receiver.recv() => match received {
                        Ok(event) if event.cursor.epoch == cursor.epoch && event.cursor.seq <= cursor.seq => continue,
                        Ok(event) => {
                            yield Ok(w::HostEvent { event: Some(w::host_event::Event::Notification(Box::new(event))), ..Default::default() });
                        }
                        Err(_) => {
                            application.unsubscribe(&subscription_id).await;
                            yield Ok(w::HostEvent { event: Some(w::host_event::Event::ResyncRequired(Box::default())), ..Default::default() });
                            break;
                        }
                    }
                }
            }
        })
    }
    async fn list_companions(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Companions> {
        Response::ok(w::Companions {
            items: self.app.application.companions().await.map_err(failure)?,
            ..Default::default()
        })
    }
    async fn get_companion(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Companion> {
        Response::ok(
            self.app
                .application
                .companion(req.id)
                .await
                .map_err(failure)?,
        )
    }
    async fn update_companion(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let input = req.to_owned_message();
        let id = input.id.clone();
        Response::ok(
            self.app
                .application
                .patch_companion(id, input)
                .await
                .map_err(failure)?,
        )
    }
    async fn get_mind(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::MindView> {
        Response::ok(
            self.app
                .application
                .get_mind(req.to_owned_message().id)
                .await
                .map_err(failure)?,
        )
    }

    async fn control_memory(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::MemoryControlRequest>,
    ) -> ServiceResult<w::MemoryControlResult> {
        let request = req.to_owned_message();
        let memory = self
            .app
            .application
            .control_memory(request.companion_id, request.memory_id, request.action)
            .await
            .map_err(failure)?;
        Response::ok(w::MemoryControlResult {
            memory_id: memory.id,
            audience: memory.audience,
            recall_preference: memory.recall_preference,
            revision: memory.revision,
            ..Default::default()
        })
    }

    async fn update_mind_profile(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateMindProfileRequest>,
    ) -> ServiceResult<w::MindView> {
        Response::ok(
            self.app
                .application
                .update_mind_profile(req.to_owned_message())
                .await
                .map_err(failure)?,
        )
    }

    async fn get_history(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::HistoryRequest>,
    ) -> ServiceResult<w::History> {
        let r = req.to_owned_message();
        let page = self
            .app
            .application
            .messages(r.companion_id, r.cursor, r.limit.unwrap_or(50) as usize)
            .await
            .map_err(failure)?;
        Response::ok(page)
    }
    async fn get_message(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::MessageRequest>,
    ) -> ServiceResult<w::Message> {
        Response::ok(
            self.app
                .application
                .message(req.companion_id.into(), req.message_id.into())
                .await
                .map_err(failure)?,
        )
    }
    async fn send_message(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::SendMessageRequest>,
    ) -> ServiceResult<w::MessageAcceptance> {
        let r = req.to_owned_message();
        Response::ok(receipt::<w::MessageAcceptance>(
            self.app
                .application
                .accept_message(r.companion_id.clone(), key(r.request_key.clone())?, r)
                .await
                .map_err(failure)?,
        )?)
    }
    async fn stop(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::StopRequest>,
    ) -> ServiceResult<w::Empty> {
        self.app
            .application
            .cancel_companion(req.companion_id.into())
            .await
            .map_err(failure)?;
        Response::ok(w::Empty::default())
    }
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        Response::ok(self.app.application.settings().await.map_err(failure)?)
    }
    async fn update_settings(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        let r = req.to_owned_message();
        Response::ok(
            self.app
                .application
                .patch_settings(r)
                .await
                .map_err(failure)?,
        )
    }
    async fn list_grants(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::GrantsRequest>,
    ) -> ServiceResult<w::Grants> {
        Response::ok(w::Grants {
            items: self
                .app
                .application
                .grants(req.companion_id)
                .await
                .map_err(failure)?
                .into_iter()
                .map(w::Grant::from)
                .collect(),
            ..Default::default()
        })
    }
    async fn create_grant(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::CreateGrantRequest>,
    ) -> ServiceResult<w::GrantResult> {
        let r = req.to_owned_message();
        Response::ok(receipt::<w::GrantResult>(
            self.app
                .application
                .post_grant(key(r.request_key.clone())?, r)
                .await
                .map_err(failure)?,
        )?)
    }
    async fn update_grant(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateGrantRequest>,
    ) -> ServiceResult<w::GrantResult> {
        let r = req.to_owned_message();
        Response::ok(
            self.app
                .application
                .patch_grant(r.id.clone(), r)
                .await
                .map_err(failure)?,
        )
    }
    async fn revoke_grant(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        self.app
            .application
            .delete_grant(req.id.into())
            .await
            .map_err(failure)?;
        Response::ok(w::Empty::default())
    }
    async fn register_credentials(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let r = req.to_owned_message();
        Response::ok(
            self.app
                .application
                .register_credentials(r.id, r.secret, r.source)
                .await
                .map_err(failure)?,
        )
    }
    async fn revoke_credentials(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        self.app
            .application
            .revoke_credentials(req.id.into())
            .await
            .map_err(failure)?;
        Response::ok(w::Empty::default())
    }
    async fn check_connection(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        let r = req.to_owned_message();
        Response::ok(receipt::<w::CheckResult>(
            self.app
                .application
                .check(r.id.clone(), key(r.request_key.clone())?, r)
                .await
                .map_err(failure)?,
        )?)
    }
    async fn list_models(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        let r = req.to_owned_message();
        Response::ok(w::Models {
            items: self
                .app
                .application
                .models(r.id, r.companion_id)
                .await
                .map_err(failure)?,
            ..Default::default()
        })
    }
    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Providers> {
        Response::ok(w::Providers {
            items: self.app.application.providers(),
            ..Default::default()
        })
    }
    async fn reload_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Providers> {
        self.app
            .application
            .reload_providers()
            .await
            .map_err(failure)?;
        Response::ok(w::Providers {
            items: self.app.application.providers(),
            ..Default::default()
        })
    }
    async fn enable_provider(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::EnableProviderRequest>,
    ) -> ServiceResult<w::Providers> {
        self.app
            .application
            .enable_provider(req.id.into(), req.enabled)
            .await
            .map_err(failure)?;
        Response::ok(w::Providers {
            items: self.app.application.providers(),
            ..Default::default()
        })
    }
    async fn get_availability(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::AvailabilityRequest>,
    ) -> ServiceResult<w::Availability> {
        Response::ok(
            self.app
                .application
                .availability(req.connection_id.into(), req.companion_id.into())
                .await
                .map_err(failure)?,
        )
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use buffa::{HasMessageView, view::MessageView};
    use futures_util::StreamExt;
    use w::HostService;

    #[tokio::test]
    async fn lag_invalidates_the_snapshot_lease_before_requesting_resync() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::infrastructure::sqlite::Store::open(temp.path().join("host.db"))
            .await
            .unwrap();
        let app = crate::bootstrap::application(store, "fixture".into());
        let ctx = RequestContext::new(axum::http::HeaderMap::new());
        let bytes = bytes::Bytes::new();
        let view = <w::Empty as HasMessageView>::View::decode_view(&bytes).unwrap();
        let service = HostRpc { app: app.clone() };
        let mut stream = service
            .subscribe(ctx, ServiceRequest::<w::Empty>::from_parts(&view, &bytes))
            .await
            .unwrap()
            .body;
        let first = stream.next().await.unwrap().unwrap();
        let Some(w::host_event::Event::Initial(initial)) = first.event else {
            panic!("initial snapshot missing")
        };
        let mut companion = app.application.companions().await.unwrap().remove(0);
        for index in 0..300 {
            companion = app
                .application
                .patch_companion(
                    companion.id.clone(),
                    w::UpdateCompanionRequest {
                        revision: companion.revision,
                        name: Some(format!("lag-{index}")),
                        mind: None,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert!(matches!(
            stream.next().await.unwrap().unwrap().event,
            Some(w::host_event::Event::ResyncRequired(_))
        ));
        assert!(
            app.application
                .snapshot(Some(&initial.subscription_id))
                .await
                .is_err()
        );
        assert!(stream.next().await.is_none());
        app.application.stop().await.unwrap();
    }
}
