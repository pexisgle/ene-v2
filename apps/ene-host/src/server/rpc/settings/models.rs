//! Settings service.

use super::super::Services;
use super::{model_item, notify_settings};
use crate::host::error::HostError;
use crate::host::model;
use crate::host::settings;
use crate::host::settings::CredentialState;

use connectrpc::ConnectError;
use connectrpc::RequestContext;
use connectrpc::Response;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;

pub(super) async fn list_providers(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::LlmProviders> {
    Response::ok(w::LlmProviders {
        items: settings::providers(&services.runtime.providers()),
        ..Default::default()
    })
}

pub(super) async fn list_models(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::ModelsRequest>,
) -> ServiceResult<w::Models> {
    let id = request.id.to_owned();
    services
        .inspect(move |services| {
            let connection = services
                .store
                .settings_repo()
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
            Ok(w::Models {
                items: model::models(&connection, &services.catalog.read())
                    .into_iter()
                    .map(|(id, spec)| model_item(id, &spec))
                    .collect(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn register_credentials(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::CredentialRequest>,
) -> ServiceResult<w::CredentialResult> {
    let id = request.id.to_owned();
    let secret = request
        .secret
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| ConnectError::invalid_argument("secret is required"))?
        .to_owned();
    services
        .change(move |services| {
            let store = &services.store;
            if store.settings_repo().connection(&id)?.is_none() {
                return Err(HostError::NotFound("connection not found".into()));
            }
            model::save_secret(&services.credentials, &id, &secret)?;
            store
                .settings_repo()
                .set_credential_state(&id, CredentialState::Registered)?;
            notify_settings(
                &services.notifier,
                w::Settings::from(&store.settings_repo().settings()?),
            );
            Ok(w::CredentialResult {
                credential_state: CredentialState::Registered.as_str().into(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn revoke_credentials(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::Id>,
) -> ServiceResult<w::Empty> {
    let id = request.id.to_owned();
    services
        .change(move |services| {
            let store = &services.store;
            let connection = store
                .settings_repo()
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
            model::delete_secret(&services.credentials, &id)?;
            let state =
                model::credential_state(&connection, &services.credentials, &services.providers)?;
            store.settings_repo().set_credential_state(&id, state)?;
            notify_settings(
                &services.notifier,
                w::Settings::from(&store.settings_repo().settings()?),
            );
            Ok(w::Empty::default())
        })
        .await
}
