//! Connection CRUD, probing, credential registration, and onboarding setup RPCs.

use super::{
    CallContext, RPC_TIMEOUT, RpcDispatcher, companion_update,
    companions::shared_model_assignments, failure, failure_notice, id_request,
};
use crate::{host::Host, update::Update};
use ene_client_core::{
    connections::build_connection_model_input,
    domain::{Companion, Connection, ConnectionInput, ModelAssignment},
};
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
    pub(crate) settings_revision: u64,
    pub(crate) conn: Option<Connection>,
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
) -> Result<bool, connectrpc::ConnectError> {
    Ok(host
        .settings
        .check_connection(check_request(connection_id, revision, ticket))
        .await?
        .view()
        .success)
}

fn dispatch_probe<F>(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    context: CallContext,
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
    let Some(host) = rpc.host() else {
        return;
    };
    for id in connection_ids {
        let request_id = id.clone();
        let host = host.clone();
        rpc.dispatch(
            epoch,
            "モデル一覧の取得".into(),
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
        return dispatch_probe(
            rpc,
            epoch,
            id,
            CallContext {
                label: context,
                timeout: RPC_TIMEOUT + PROBE_TIMEOUT,
            },
            move |host, ticket| async move {
                let save = before_probe(host.settings.update_settings(request)).await?;
                run_connection_probe(host, named, save.view().revision, ticket).await
            },
        );
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

async fn before_probe<T>(
    request: impl Future<Output = Result<T, connectrpc::ConnectError>>,
) -> Result<T, connectrpc::ConnectError> {
    tokio::time::timeout(RPC_TIMEOUT, request)
        .await
        .map_err(|_| {
            connectrpc::ConnectError::deadline_exceeded("the connection setup timed out")
        })?
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
        CallContext {
            label: "MCP の追加",
            timeout: 3 * RPC_TIMEOUT + PROBE_TIMEOUT,
        },
        move |host, ticket| async move {
            let mut create = input.clone();
            create.enabled = secret.is_empty();
            let saved = before_probe(
                host.settings
                    .update_settings(settings_request(revision, vec![create])),
            )
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
                before_probe(host.settings.register_credentials(credential_request(
                    named.clone(),
                    secret,
                    Some(revision),
                )))
                .await?;
                input.enabled = true;
                revision = before_probe(
                    host.settings
                        .update_settings(settings_request(revision, vec![input])),
                )
                .await?
                .view()
                .revision;
            }
            run_connection_probe(host, named, revision, ticket).await
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

fn check_request(id: String, revision: u64, request_key: String) -> w::CheckRequest {
    w::CheckRequest {
        id,
        request_key,
        expected_settings_revision: Some(revision),
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
) -> Option<String> {
    let named = connection_id.clone();
    dispatch_probe(
        rpc,
        epoch,
        connection_id,
        CallContext {
            label: "接続確認",
            timeout: PROBE_TIMEOUT,
        },
        |host, ticket| run_connection_probe(host, named, revision, ticket),
    )
}

pub(crate) fn register_secret(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    secret: String,
    revision: Option<u64>,
) {
    let request = credential_request(connection_id.clone(), secret, revision);
    rpc.call_owned(
        epoch,
        "資格情報の登録",
        request!(settings.register_credentials(request)),
        move |_| Update::CredentialRegistered { connection_id },
    );
}

pub(crate) fn register_secret_for_connections(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_ids: Vec<String>,
    secret: String,
    revision: u64,
) {
    rpc.call(
        epoch,
        "複数接続への資格情報登録",
        |host| async move {
            for id in connection_ids {
                host.settings
                    .register_credentials(credential_request(id, secret.clone(), Some(revision)))
                    .await?;
            }
            Ok::<_, connectrpc::ConnectError>(())
        },
        |_| Update::Resnapshot,
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
        CallContext {
            label: "接続設定の保存",
            timeout: 2 * RPC_TIMEOUT,
        },
        |host| async move {
            let revision = if req.create {
                before_probe(
                    host.settings
                        .update_settings(settings_request(req.revision, vec![req.input])),
                )
                .await?
                .view()
                .revision
            } else {
                req.revision
            };
            if !req.secret.is_empty() {
                before_probe(host.settings.register_credentials(credential_request(
                    req.id.clone(),
                    req.secret,
                    Some(revision),
                )))
                .await?;
            }
            Ok(req.id)
        },
        |id| Update::SetupSaved(Ok(id)),
        |label, error| Update::SetupSaved(Err(failure_notice(label, error))),
    )
}

pub(crate) fn complete_setup_selection(rpc: &RpcDispatcher, epoch: u64, selection: SetupSelection) {
    rpc.call(
        epoch,
        "初期設定の完了",
        |host| async move {
            if let Some(conn) = selection.conn {
                let input = build_connection_model_input(&conn, &selection.model_id);
                let _written = host
                    .settings
                    .update_settings(settings_request(selection.settings_revision, vec![input]))
                    .await?;
            }
            let patch = w::AssignmentPatch::from(Some(ModelAssignment {
                connection_id: selection.connection_id.clone(),
                model_id: selection.model_id.clone(),
            }));
            if let Some(comp) = selection.existing_comp {
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: comp.id,
                        revision: comp.revision,
                        companion_assignment: Some(patch.clone()).into(),
                        task_assignment: Some(patch).into(),
                        voice_identifier: selection.voice_identifier,
                        ..Default::default()
                    })
                    .await?;
                Ok(companion_update(response.into_owned()))
            } else {
                let assignments = shared_model_assignments(ModelAssignment {
                    connection_id: selection.connection_id,
                    model_id: selection.model_id,
                });
                let response = host
                    .host
                    .create_companion(w::CreateCompanionRequest {
                        name: selection.companion_name,
                        mind: "相手を思いやりながら率直に対話する。".into(),
                        persona: Some("柔らかく親しみやすい口調で会話します。".into()),
                        voice_identifier: selection.voice_identifier,
                        assignments: Some(assignments).into(),
                        ..Default::default()
                    })
                    .await?;
                let created = response.into_owned();
                Ok(Update::Created(created.id))
            }
        },
        |update| update,
    );
}
