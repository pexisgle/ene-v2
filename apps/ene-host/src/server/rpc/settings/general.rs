//! Settings service.

use super::super::Services;
use super::notify_settings;
use crate::host::error::HostError;
use crate::host::settings;
use crate::host::voice::start_selected_voicevox;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;

pub(super) async fn get_settings(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::Settings> {
    let providers = services.runtime.providers();
    services
        .with_store(move |store| {
            Ok(settings::project(
                &store.settings_repo().settings()?,
                &providers,
            ))
        })
        .await
}

pub(super) async fn update_settings(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::UpdateSettingsRequest>,
) -> ServiceResult<w::Settings> {
    let request = request.to_owned_message();
    let expected_revision = request.expected_revision;
    let mut inputs = Vec::with_capacity(request.connections.len());
    for input in request.connections {
        let expected = input.expected_revision;
        inputs.push((
            ene_domain::Connection::try_from(input).map_err(HostError::from)?,
            expected,
        ));
    }
    let view = services
        .change(move |services| {
            let store = &services.store;
            let settings = store
                .settings_repo()
                .update_settings(expected_revision, &inputs)?;
            let view = settings::project(&settings, &services.providers);
            notify_settings(&services.notifier, view.clone());
            Ok(view)
        })
        .await?;
    // The save lands first. A plugin starts because a stored connection
    // names it. `start_used` logs a plugin that will not start and carries
    // on, the same policy as startup, so the save still succeeds.
    let services = services.runtime.services();
    let store = &services.store;
    services.providers.start_used(store).await;
    if let Ok(current) = store.settings_repo().settings()
        && !start_selected_voicevox(&current, &services.providers, &services.voicevox)
    {
        services.voicevox.stop().await;
    }
    Ok(view)
}
