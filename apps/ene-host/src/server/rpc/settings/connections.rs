//! Settings service.

use super::super::Services;
use super::connection_probe::run_connection_probe;
use super::{notify_settings, parse_connection_id};
use crate::host::error::HostError;
use crate::host::mcp::ConnectionStamp;
use crate::host::permission;
use crate::host::settings;
use crate::host::voice::is_builtin_voicevox;
use crate::host::wire;
use crate::host::{mind::ModelAssignment, model};
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
    let id = parse_connection_id(request.id)?;
    let revision = request.expected_revision;
    let dependencies = services.runtime.services().clone();
    let response = services
        .call(move |kernel| {
            let store = &dependencies.store;
            let (settings, companions) = store.settings_repo().delete_connection(&id, revision)?;
            dependencies.credentials.forget(id.as_str());
            let view = settings::project(&settings, &dependencies.providers);
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
    services
        .with_vault(|host| host.credentials.recover_detached())
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
                .any(|connection| {
                    connection.enabled && is_builtin_voicevox(connection, &services.providers)
                }))
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
    let id = parse_connection_id(request.id)?;
    let expected = request.expected_settings_revision;
    let resource_id = request.resource_id.map(str::to_owned);
    let requested = resource_id.clone();
    let dependencies = services.runtime.services().clone();
    let (connection, stamp, plugin_revision, selected_model) = services
        .runtime
        .read_kernel(move |kernel| {
            let repo = dependencies.store.settings_repo();
            settings::validate_settings_revision(&repo, expected)?;
            let (connection, created) = repo
                .connection_with_creation(id.as_str())?
                .ok_or_else(|| HostError::NotFound(("connection not found").into()))?;
            let plugin_revision = matches!(
                connection.kind,
                settings::ConnectionKind::Acp
                    | settings::ConnectionKind::Llm
                    | settings::ConnectionKind::Stt
                    | settings::ConnectionKind::Tts
                    | settings::ConnectionKind::Embedding
            )
            .then(|| dependencies.providers.revision());
            let model = if connection.kind == settings::ConnectionKind::Llm
                && let Some(model_id) = requested
            {
                if model_id.trim().is_empty() || model_id.chars().count() > 512 {
                    return Err(HostError::Invalid(
                        "a model id of 1 to 512 characters is required".into(),
                    ));
                }
                let assignment = ModelAssignment {
                    connection_id: id.to_string(),
                    model_id,
                };
                let model = kernel.connect_model(&assignment);
                Some((model, assignment.model_id))
            } else {
                None
            };
            let stamp = ConnectionStamp {
                revision: connection.revision,
                created_revision: created,
                credential_generation: dependencies.credentials.generation_of(id.as_str()),
            };
            Ok((connection, stamp, plugin_revision, model))
        })
        .await
        .map_err(ConnectError::from)?;
    let check = match selected_model {
        Some((model, model_id)) => model::probe_model(model, model_id).await,
        None => {
            run_connection_probe(
                services,
                &services.runtime.services().credentials,
                &connection,
                stamp.created_revision,
                stamp.credential_generation,
            )
            .await?
        }
    };
    let request_id = request.request_key.to_owned();
    let kind = connection.kind;
    let id = connection.id;
    services
        .change(move |services| {
            if services.credentials.generation_of(id.as_str()) != stamp.credential_generation {
                return Err(HostError::Conflict(
                    "credentials changed during the check".into(),
                ));
            }
            if plugin_revision.is_some_and(|revision| revision != services.providers.revision()) {
                return Err(HostError::Conflict(
                    "plugins changed during the check".into(),
                ));
            }
            let repo = services.store.settings_repo();
            let stored = if kind == settings::ConnectionKind::Llm {
                let previous = repo
                    .connection(id.as_str())?
                    .and_then(|connection| connection.last_check);
                combine_llm_checks(previous, &check)
            } else {
                check.clone()
            };
            repo.set_check(&id, stamp.revision, stamp.created_revision, &stored)?;
            notify_settings(
                &services.notifier,
                settings::project(&repo.settings()?, &services.providers),
            );
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

// A listing refresh and a selected-model check record different facts.
fn combine_llm_checks(
    previous: Option<settings::Check>,
    check: &settings::Check,
) -> settings::Check {
    let Some(previous) = previous else {
        return check.clone();
    };
    if check.checked_model.is_some() {
        return settings::Check {
            capabilities: previous.capabilities,
            resources: previous.resources,
            ..check.clone()
        };
    }
    if previous.checked_model.is_none() {
        return check.clone();
    }
    if !check.healthy {
        return previous;
    }
    settings::Check {
        capabilities: check.capabilities.clone(),
        resources: check.resources.clone(),
        ..previous
    }
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
