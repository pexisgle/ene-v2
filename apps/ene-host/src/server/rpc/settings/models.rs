//! Settings service.

use super::super::Services;
use super::{model_item, notify_settings, parse_connection_id};
use crate::host::model;
use crate::host::settings;
use crate::host::settings::CredentialState;
use crate::host::{HostServices, error::Result};

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
    let id = parse_connection_id(request.id)?;
    services
        .read(move |kernel| {
            Ok(w::Models {
                items: kernel
                    .list_models(&id)?
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
    let id = parse_connection_id(request.id)?;
    let expected = request.expected_settings_revision;
    let secret = request
        .secret
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| ConnectError::invalid_argument("secret is required"))?
        .to_owned();
    change_credentials(services, move |host| {
        model::save_secret(&host.credentials, id.as_str(), &secret, expected)?;
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
    let id = parse_connection_id(request.id)?;
    change_credentials(services, move |host| {
        model::delete_secret(&host.credentials, id.as_str())?;
        Ok(w::Empty::default())
    })
    .await
}

async fn change_credentials<T: Send + 'static>(
    services: &Services,
    operation: impl FnOnce(&HostServices) -> Result<T> + Send + 'static,
) -> ServiceResult<T> {
    let result = services.with_vault(operation).await?;
    services
        .change(move |host| {
            let response = result?;
            notify_settings(
                &host.notifier,
                settings::project(&host.store.settings_repo().settings()?, &host.providers),
            );
            Ok(response)
        })
        .await
}
