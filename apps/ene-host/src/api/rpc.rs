//! Generated service implementations. Storage/plugin DTOs end at this boundary.
use super::{error::ApiError, state::App};
use connectrpc::{
    ConnectError, ErrorCode, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_api_types::{host as h, rpc as w, rpc_conversion::RpcConvert};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn service(app: App) -> connectrpc::ConnectRpcService {
    connectrpc::Router::new()
        .add_service(Arc::new(HostRpc { app: app.clone() }))
        .add_service(Arc::new(super::plugin_rpc::PluginRpc { app: app.clone() }))
        .add_service(Arc::new(super::device_rpc::DeviceRpc { app }))
        .into_axum_service()
        .with_limits(
            connectrpc::Limits::default()
                .with_max_message_size(16 * 1024 * 1024)
                .with_max_request_body_size(16 * 1024 * 1024),
        )
}
struct HostRpc {
    app: App,
}
pub(super) fn failure(error: ene_core::domain::Error) -> ConnectError {
    ApiError::from(error).into_rpc()
}
pub(super) fn invalid() -> ConnectError {
    ApiError::invalid().into_rpc()
}
fn wire<T: RpcConvert>(value: T) -> Result<T::Wire, ConnectError> {
    value
        .into_wire()
        .map_err(|_| ConnectError::internal("応答を変換できません"))
}
fn read<T: RpcConvert>(value: T::Wire) -> Result<T, ConnectError> {
    T::from_wire(value).map_err(|_| invalid())
}
fn key(value: String) -> Result<String, ConnectError> {
    if value.is_empty() || value.len() > 200 || value.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(invalid());
    }
    Ok(value)
}
fn receipt<T: serde::de::DeserializeOwned + RpcConvert>(
    (outcome, value): (ene_core::ports::Outcome, serde_json::Value),
) -> Result<T::Wire, ConnectError> {
    use ene_core::ports::Outcome;
    if matches!(outcome, Outcome::Pending | Outcome::Unavailable) {
        let body: h::ApiErrorBody = serde_json::from_value(value)
            .map_err(|_| ConnectError::internal("応答を変換できません"))?;
        let status = if matches!(outcome, Outcome::Pending) {
            409
        } else {
            503
        };
        return Err(ConnectError::new(
            if status == 409 {
                ErrorCode::FailedPrecondition
            } else {
                ErrorCode::Unavailable
            },
            &body.error.message,
        )
        .with_detail(connectrpc::ErrorDetail::from_message(
            "ene.host.v1.ApiFailure",
            &w::ApiFailure {
                error: wire(body.error)?.into(),
                request_id: body.request_id,
                http_status: status,
                ..Default::default()
            },
        )));
    }
    wire(
        serde_json::from_value::<T>(value)
            .map_err(|_| ConnectError::internal("応答を変換できません"))?,
    )
}
fn assignments(
    input: w::UpdateCompanionRequest,
) -> Result<(String, h::CompanionPatch), ConnectError> {
    let mut changes = BTreeMap::new();
    for (slot, change) in [
        ("companion", input.companion_assignment),
        ("task", input.task_assignment),
    ] {
        if let Some(change) = change.into_option() {
            let value = match change.change.ok_or_else(invalid)? {
                w::assignment_patch::Change::Set(value) => {
                    Some(read::<h::ModelAssignment>(*value)?)
                }
                w::assignment_patch::Change::Clear(_) => None,
            };
            changes.insert(slot.into(), value);
        }
    }
    Ok((
        input.id,
        h::CompanionPatch {
            revision: input.revision,
            name: input.name,
            mind: input.mind,
            model_assignments: if changes.is_empty() {
                None
            } else {
                Some(changes)
            },
        },
    ))
}

#[allow(refining_impl_trait)]
impl w::HostService for HostRpc {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::SnapshotRequest>,
    ) -> ServiceResult<w::Snapshot> {
        Response::ok(wire(
            self.app
                .snapshot_document(req.subscription_id)
                .await
                .map_err(ApiError::into_rpc)?,
        )?)
    }
    async fn subscribe(
        &self,
        ctx: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<ServiceStream<w::HostEvent>> {
        let principal = ctx
            .extensions()
            .get::<super::auth::Principal>()
            .cloned()
            .ok_or_else(|| ConnectError::unauthenticated("認証が必要です"))?;
        let cancelled = super::auth::cancelled(principal);
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
            snapshot: wire(snapshot)?.into(),
            ..Default::default()
        };
        let mut shutdown = self.app.application.shutdown();
        Response::stream_ok(async_stream::stream! {
            let _lease = subscription.lease;
            yield Ok(w::HostEvent { event: Some(w::host_event::Event::Initial(Box::new(initial))), ..Default::default() });
            tokio::pin!(cancelled);
            loop {
                if *shutdown.borrow() { break; }
                tokio::select! {
                    _ = &mut cancelled => break,
                    _ = shutdown.changed() => break,
                    received = subscription.receiver.recv() => match received {
                        Ok(event) if event.epoch == cursor.epoch && event.seq <= cursor.seq => continue,
                        Ok(event) => {
                            match wire(h::Notification {epoch:event.epoch,seq:event.seq,kind:event.kind,companion_id:event.companion_id,data:event.data}) {
                                Ok(event) => yield Ok(w::HostEvent { event: Some(w::host_event::Event::Notification(Box::new(event))), ..Default::default() }),
                                Err(error) => { yield Err(error); break; }
                            }
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
            items: self
                .app
                .application
                .companions()
                .await
                .map_err(failure)?
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    async fn get_companion(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Companion> {
        Response::ok(wire(
            self.app
                .application
                .companion(req.id)
                .await
                .map_err(failure)?,
        )?)
    }
    async fn update_companion(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let (id, input) = assignments(req.to_owned_message())?;
        Response::ok(wire(
            self.app
                .application
                .patch_companion(id, input)
                .await
                .map_err(failure)?,
        )?)
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
        Response::ok(w::History {
            items: page.items.into_iter().map(wire).collect::<Result<_, _>>()?,
            next_cursor: page.next_cursor,
            ..Default::default()
        })
    }
    async fn get_message(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::MessageRequest>,
    ) -> ServiceResult<w::Message> {
        Response::ok(wire(
            self.app
                .application
                .message(req.companion_id.into(), req.message_id.into())
                .await
                .map_err(failure)?,
        )?)
    }
    async fn send_message(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::SendMessageRequest>,
    ) -> ServiceResult<w::MessageAcceptance> {
        let r = req.to_owned_message();
        let input = h::MessageInput {
            text: r.text,
            resources: r
                .resources
                .into_iter()
                .map(read::<serde_json::Value>)
                .collect::<Result<_, _>>()?,
            question_id: r.question_id,
            decision: r.decision,
        };
        Response::ok(receipt::<h::MessageAcceptance>(
            self.app
                .application
                .accept_message(r.companion_id, key(r.request_key)?, input)
                .await
                .map_err(failure)?,
        )?)
    }
    async fn stop(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::StopRequest>,
    ) -> ServiceResult<w::Empty> {
        self.app.audio.stop(req.companion_id).await;
        self.app
            .application
            .cancel_companion(req.companion_id.into())
            .await
            .map_err(failure)?;
        Response::ok(w::Empty::default())
    }
    async fn prepare_audio(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, w::PrepareAudioRequest>,
    ) -> ServiceResult<w::AudioStart> {
        super::auth::own_device(&ctx, req.device_id)?;
        self.app
            .application
            .companion(req.companion_id)
            .await
            .map_err(failure)?;
        self.app
            .devices
            .permitted(req.device_id, "audio.input")
            .await?;
        self.app
            .devices
            .permitted(req.device_id, "audio.output")
            .await?;
        let permission = self.app.devices.changes(req.device_id).await?;
        Response::ok(
            self.app
                .audio
                .prepare(req.to_owned_message(), permission)
                .await?,
        )
    }
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        Response::ok(wire(
            self.app.application.settings().await.map_err(failure)?,
        )?)
    }
    async fn update_settings(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        let r = req.to_owned_message();
        let connections = r
            .connections
            .into_iter()
            .map(|r| {
                Ok(h::ConnectionInput {
                    id: r.id,
                    create: r.create,
                    kind: r.kind,
                    adapter: r.adapter,
                    endpoint: r.endpoint,
                    options: read::<serde_json::Value>(
                        r.options.into_option().ok_or_else(invalid)?,
                    )?,
                })
            })
            .collect::<Result<_, ConnectError>>()?;
        Response::ok(wire(
            self.app
                .application
                .patch_settings(h::SettingsPatch {
                    revision: r.revision,
                    connections,
                })
                .await
                .map_err(failure)?,
        )?)
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
                .map(|g| wire(h::DataTransferGrant::from(g)))
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    async fn create_grant(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::CreateGrantRequest>,
    ) -> ServiceResult<w::GrantResult> {
        let r = req.to_owned_message();
        let consent = r.consent.into_option().ok_or_else(invalid)?;
        let input = h::GrantInput {
            companion_id: r.companion_id,
            kind: r.kind,
            connection_id: r.connection_id,
            destinations: r.destinations,
            data_kinds: r.data_kinds,
            purpose: r.purpose,
            consent: h::GrantConsent {
                settings_revision: consent.settings_revision,
                provider_revision: consent.provider_revision,
            },
        };
        Response::ok(receipt::<h::GrantResult>(
            self.app
                .application
                .post_grant(key(r.request_key)?, input)
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
        Response::ok(wire(
            self.app
                .application
                .patch_grant(
                    r.id,
                    h::GrantPatch {
                        revision: r.revision,
                        destinations: r.destinations.into_option().map(|v| v.values),
                        data_kinds: r.data_kinds.into_option().map(|v| v.values),
                    },
                )
                .await
                .map_err(failure)?,
        )?)
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
        Response::ok(wire(
            self.app
                .application
                .register_credentials(r.id, r.secret, r.source)
                .await
                .map_err(failure)?,
        )?)
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
        Response::ok(receipt::<h::CheckResult>(
            self.app
                .application
                .check(
                    r.id,
                    key(r.request_key)?,
                    h::CheckInput {
                        companion_id: r.companion_id,
                        purpose: r.purpose,
                        model_id: r.model_id,
                    },
                )
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
                .map_err(failure)?
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Providers> {
        Response::ok(w::Providers {
            items: self
                .app
                .application
                .providers()
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
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
            items: self
                .app
                .application
                .providers()
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
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
            items: self
                .app
                .application
                .providers()
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    async fn get_availability(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::AvailabilityRequest>,
    ) -> ServiceResult<w::Availability> {
        Response::ok(wire(
            self.app
                .application
                .availability(req.connection_id.into(), req.companion_id.into())
                .await
                .map_err(failure)?,
        )?)
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
        let mut ctx = RequestContext::new(axum::http::HeaderMap::new());
        ctx.extensions_mut()
            .insert(super::super::auth::Principal::Owner);
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
                    h::CompanionPatch {
                        revision: companion.revision,
                        name: Some(format!("lag-{index}")),
                        mind: None,
                        model_assignments: None,
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
