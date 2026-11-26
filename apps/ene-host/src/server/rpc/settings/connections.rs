//! Settings service.

use super::super::Services;
use super::connection_probe::run_connection_probe;
use super::{is_builtin_voicevox, notify_settings};
use crate::host::error::HostError;
use crate::host::model;
use crate::host::permission;
use crate::host::settings;
use crate::host::wire;
use connectrpc::ConnectError;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;

pub(super) async fn delete_connection(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::DeleteConnectionRequest>,
) -> ServiceResult<w::Settings> {
    let id = request.id.to_owned();
    let revision = request.expected_revision;
    let dependencies = services.runtime.services().clone();
    let response = services
        .call(move |kernel| {
            let store = &dependencies.store;
            let (settings, companions) =
                store.settings_repo().delete_connection(&id, revision, || {
                    crate::host::model::delete_secret(&dependencies.credentials, &id)
                })?;
            let view = wire::settings(&settings);
            notify_settings(&dependencies.notifier, view.clone());
            for companion in companions {
                if let Err(error) = kernel.rebind_companion(&companion.id) {
                    tracing::warn!(%error, "could not rebind companion after connection removal");
                }
                crate::host::mind::notify(&dependencies.notifier, &companion);
            }
            Ok(view)
        })
        .await?;
    let has_voicevox = services
        .runtime
        .inspect(|services| {
            Ok(services
                .store
                .settings_repo()
                .settings()?
                .connections
                .iter()
                .any(|connection| is_builtin_voicevox(connection, &services.providers)))
        })
        .await;
    if matches!(has_voicevox, Ok(false)) {
        services.runtime.voicevox().stop().await;
    }
    Ok(response)
}

pub(super) async fn check_connection(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::CheckRequest>,
) -> ServiceResult<w::CheckResult> {
    let id = request.id.to_owned();
    let (connection, store) = services
        .runtime
        .inspect(move |services| {
            let store = services.store.clone();
            let connection = store
                .settings_repo()
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound(("connection not found").into()))?;
            Ok((connection, store))
        })
        .await
        .map_err(ConnectError::from)?;
    let credentials = services.runtime.credentials();
    let agent_platform = connection.endpoint().adapter == model::agent_platform::ADAPTER;
    let is_mcp = connection.kind == settings::ConnectionKind::Mcp;
    let is_search = connection.kind == settings::ConnectionKind::Search;
    let checked_revision = (agent_platform || is_mcp || is_search).then_some(connection.revision);
    let checked_credentials = (agent_platform || is_search).then(|| credentials.generation());
    // The probe performs a network request, so it must not run on the ECS
    // thread; the connection was captured above.
    let check = run_connection_probe(services, &store, &credentials, &connection).await?;
    let request_id = request.request_key.to_owned();
    let resource_id = request.resource_id.map(str::to_owned);
    let id = connection.id;
    services
        .change(move |services| {
            let store = &services.store;
            if let Some(revision) = checked_revision
                && (checked_credentials
                    .is_some_and(|generation| credentials.generation() != generation)
                    || store
                        .settings_repo()
                        .connection(&id)?
                        .is_some_and(|current| current.revision != revision))
            {
                return Err(HostError::Conflict(
                    "connection changed during the check".into(),
                ));
            }
            store.settings_repo().set_check(&id, &check)?;
            if checked_revision.is_some() {
                notify_settings(
                    &services.notifier,
                    wire::settings(&store.settings_repo().settings()?),
                );
            }
            Ok(w::CheckResult {
                request_id,
                success: check.healthy,
                resource_id,
                check: Some((&check).into()).into(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn list_pending_approvals(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::PendingApprovals> {
    services
        .with_store(|store| {
            Ok(w::PendingApprovals {
                items: store
                    .approval_repo()
                    .approvals(permission::ApprovalStatus::Pending)?
                    .iter()
                    .map(wire::approval)
                    .collect(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn submit_approval(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::SubmitApprovalRequest>,
) -> ServiceResult<w::SubmitApprovalResponse> {
    let decision = match request.decision.as_known() {
        Some(w::ApprovalDecision::Approve) => permission::ApprovalDecision::Approve,
        Some(w::ApprovalDecision::AlwaysApprove) => permission::ApprovalDecision::AlwaysApprove,
        Some(w::ApprovalDecision::Deny) => permission::ApprovalDecision::Deny,
        Some(w::ApprovalDecision::Unspecified) | None => permission::ApprovalDecision::Unspecified,
    };
    let request_id = request.request_id.to_owned();
    services
        .call(move |kernel| kernel.submit_approval(&request_id, decision))
        .await
}
