//! Connection CRUD, probing, credential registration, and onboarding setup RPCs.

use super::{
    RpcDispatcher, companions::shared_model_assignments, failure, failure_notice, id_request,
};
use crate::{host::Host, update::Update};
use connectrpc::client::CallOptions;
use ene_client_core::domain::{Companion, ConnectionInput, ModelAssignment};
use ene_rpc as w;
use std::{future::Future, time::Duration};

const PROBE_TIMEOUT: Duration = Duration::from_secs(65);

/// Connection and optional credential to save before onboarding model selection.
pub(crate) struct SetupConnection {
    pub(crate) revision: u64,
    pub(crate) id: String,
    pub(crate) create: bool,
    pub(crate) input: ConnectionInput,
    pub(crate) secret: String,
}

/// Parameters for finalizing onboarding setup selection.
pub(crate) struct SetupSelection {
    pub(crate) connection_id: String,
    pub(crate) model_id: String,
    pub(crate) existing_comp: Option<Companion>,
    pub(crate) companion_name: String,
    pub(crate) voice_identifier: Option<String>,
}

async fn run_connection_probe(
    host: Host,
    connection_id: String,
    revision: u64,
    ticket: String,
    resource_id: Option<String>,
) -> Result<bool, connectrpc::ConnectError> {
    let response = host
        .settings
        .check_connection_with_options(
            w::CheckRequest {
                id: connection_id,
                request_key: ticket,
                expected_settings_revision: Some(revision),
                resource_id,
                ..Default::default()
            },
            CallOptions::default().with_timeout(PROBE_TIMEOUT),
        )
        .await?;
    Ok(response.view().success)
}

fn dispatch_probe<F>(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    context: &'static str,
    run: impl FnOnce(Host, String) -> F,
) -> Option<String>
where
    F: Future<Output = Result<bool, connectrpc::ConnectError>> + Send + 'static,
{
    let failed_id = connection_id.clone();
    let ticket = uuid::Uuid::new_v4().to_string();
    let success_ticket = ticket.clone();
    let failed_ticket = ticket.clone();
    rpc.call_with(
        epoch,
        context,
        |host| run(host, ticket.clone()),
        move |check| Update::Checked {
            connection_id,
            ticket: success_ticket,
            result: Ok(check),
        },
        move |label, error| Update::Checked {
            connection_id: failed_id,
            ticket: failed_ticket,
            result: Err(failure_notice(label, error)),
        },
    )
    .then_some(ticket)
}

pub(crate) fn load_providers(rpc: &RpcDispatcher, epoch: u64) {
    rpc.call(
        epoch,
        "プロバイダ一覧の取得",
        request!(settings.list_providers(w::Empty::default())),
        |providers| Update::Providers(providers.view().items.iter().map(Into::into).collect()),
    );
}

pub(crate) fn load_models(rpc: &RpcDispatcher, epoch: u64, connection_ids: Vec<String>) {
    let Some(host) = rpc.unary_host() else {
        return;
    };
    for id in connection_ids {
        let request_id = id.clone();
        let host = host.clone();
        rpc.dispatch(
            epoch,
            "モデル一覧の取得",
            async move {
                host.settings
                    .list_models(w::ModelsRequest {
                        id: request_id,
                        ..Default::default()
                    })
                    .await
            },
            move |models| Update::Models {
                connection_id: id,
                models: models.view().items.iter().map(Into::into).collect(),
            },
            failure,
        );
    }
}

pub(crate) fn update_settings(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    expected_revision: u64,
    inputs: Vec<ConnectionInput>,
    probe: Option<String>,
) -> Option<String> {
    let request = settings_request(expected_revision, inputs);
    if let Some(id) = probe {
        let named = id.clone();
        return dispatch_probe(rpc, epoch, id, context, move |host, ticket| async move {
            let save = host.settings.update_settings(request).await?;
            run_connection_probe(host, named, save.view().revision, ticket, None).await
        });
    }
    rpc.call_with(
        epoch,
        context,
        request!(settings.update_settings(request)),
        |_| Update::Resnapshot,
        failure,
    );
    None
}

pub(crate) fn add_mcp_and_check(
    rpc: &RpcDispatcher,
    epoch: u64,
    revision: u64,
    mut input: ConnectionInput,
    secret: String,
) -> Option<String> {
    let id = input.id.clone();
    let named = id.clone();
    dispatch_probe(
        rpc,
        epoch,
        id,
        "MCP の追加",
        move |host, ticket| async move {
            let mut create = input.clone();
            create.enabled = secret.is_empty();
            let saved = host
                .settings
                .update_settings(settings_request(revision, vec![create]))
                .await?;
            let mut revision = saved.view().revision;
            if !secret.is_empty() {
                input.expected_revision = saved
                    .view()
                    .connections
                    .iter()
                    .find(|connection| connection.id == named)
                    .ok_or_else(|| {
                        connectrpc::ConnectError::not_found("the saved MCP connection is missing")
                    })?
                    .revision;
                host.settings
                    .register_credentials(credential_request(named.clone(), secret, Some(revision)))
                    .await?;
                input.enabled = true;
                revision = host
                    .settings
                    .update_settings(settings_request(revision, vec![input]))
                    .await?
                    .view()
                    .revision;
            }
            run_connection_probe(host, named, revision, ticket, None).await
        },
    )
}

fn settings_request(revision: u64, inputs: Vec<ConnectionInput>) -> w::UpdateSettingsRequest {
    w::UpdateSettingsRequest {
        expected_revision: revision,
        connections: inputs.into_iter().map(Into::into).collect(),
        ..Default::default()
    }
}

fn credential_request(id: String, secret: String, revision: Option<u64>) -> w::CredentialRequest {
    w::CredentialRequest {
        id,
        secret: Some(secret),
        expected_settings_revision: revision,
        ..Default::default()
    }
}

pub(crate) fn delete_connection(rpc: &RpcDispatcher, epoch: u64, id: String, revision: u64) {
    rpc.call(
        epoch,
        "接続の削除",
        request!(settings.delete_connection(w::DeleteConnectionRequest {
            id,
            expected_revision: revision,
            ..Default::default()
        })),
        |_| Update::Resnapshot,
    );
}

pub(crate) fn probe_connection(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    revision: u64,
    resource_id: Option<String>,
) -> Option<String> {
    let named = connection_id.clone();
    dispatch_probe(rpc, epoch, connection_id, "接続確認", |host, ticket| {
        run_connection_probe(host, named, revision, ticket, resource_id)
    })
}

pub(crate) fn register_secret(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    secret: String,
    revision: u64,
) {
    let request = credential_request(connection_id.clone(), secret, Some(revision));
    rpc.call_owned(
        epoch,
        "資格情報の登録",
        request!(settings.register_credentials(request)),
        move |_| Update::CredentialRegistered { connection_id },
    );
}

pub(crate) fn revoke_secret(rpc: &RpcDispatcher, epoch: u64, connection_id: String) {
    rpc.call(
        epoch,
        "資格情報の削除",
        |host| async move {
            host.settings
                .revoke_credentials(id_request(connection_id))
                .await
        },
        |_| Update::Resnapshot,
    );
}

pub(crate) fn save_setup_connection(rpc: &RpcDispatcher, epoch: u64, req: SetupConnection) -> bool {
    rpc.call_with(
        epoch,
        "接続設定の保存",
        |host| async move {
            let revision = if req.create {
                host.settings
                    .update_settings(settings_request(req.revision, vec![req.input]))
                    .await?
                    .view()
                    .revision
            } else {
                req.revision
            };
            if !req.secret.is_empty() {
                host.settings
                    .register_credentials(credential_request(
                        req.id.clone(),
                        req.secret,
                        Some(revision),
                    ))
                    .await?;
            }
            Ok(req.id)
        },
        |id| Update::SetupSaved(Ok(id)),
        |label, error| Update::SetupSaved(Err(failure_notice(label, error))),
    )
}

pub(crate) fn complete_setup_selection(
    rpc: &RpcDispatcher,
    epoch: u64,
    selection: SetupSelection,
) -> bool {
    rpc.call_with(
        epoch,
        "モデルの割当",
        |host| async move {
            let assignment = ModelAssignment {
                connection_id: selection.connection_id,
                model_id: selection.model_id,
            };
            if let Some(comp) = selection.existing_comp {
                let task_is_unassigned = comp
                    .assignments
                    .as_ref()
                    .is_none_or(|assignments| assignments.task.connection_id.is_empty());
                let patch = w::AssignmentPatch::from(Some(assignment));
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: comp.id,
                        revision: comp.revision,
                        companion_assignment: Some(patch.clone()).into(),
                        task_assignment: task_is_unassigned.then_some(patch).into(),
                        voice_identifier: selection.voice_identifier,
                        ..Default::default()
                    })
                    .await?;
                Ok(response.view().id.to_owned())
            } else {
                let response = host
                    .host
                    .create_companion(w::CreateCompanionRequest {
                        name: selection.companion_name,
                        mind: "相手を思いやりながら率直に対話する。".into(),
                        persona: Some("柔らかく親しみやすい口調で会話します。".into()),
                        voice_identifier: selection.voice_identifier,
                        assignments: Some(shared_model_assignments(assignment)).into(),
                        ..Default::default()
                    })
                    .await?;
                Ok(response.view().id.to_owned())
            }
        },
        |id| Update::SetupApplied(Ok(id)),
        |label, error| Update::SetupApplied(Err(failure_notice(label, error))),
    )
}
