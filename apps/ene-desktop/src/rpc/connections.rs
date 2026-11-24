//! Connection CRUD, probing, credential registration, and onboarding setup RPCs.

use super::{RpcDispatcher, decode, failure, settings_update};
use crate::{host::Host, update::Update};
use buffa::view::MessageView;
use ene_client_core::{
    connections::build_connection_model_input,
    domain::{Companion, Connection, ConnectionInput, HostNotification, ModelAssignment},
    wire::companion::assignment_patch_set,
};
use ene_rpc as w;
use tokio::sync::mpsc::UnboundedSender;

/// Parameters for quick-connecting and verifying an LLM provider during onboarding.
pub(crate) struct QuickConnectRequest {
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

async fn run_connection_probe(host: Host, connection_id: String) -> Update {
    match host
        .settings
        .check_connection(w::CheckRequest {
            id: connection_id.clone(),
            request_key: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(resp) => match resp.view().to_owned_message() {
            Ok(res) => Update::Checked {
                connection_id,
                check: res.check.into_option().unwrap_or_default().into(),
            },
            Err(err) => failure("接続確認", decode(err)),
        },
        Err(err) => failure("接続確認", err),
    }
}

fn spawn_connection_probe(host: Host, connection_id: String, updates: UnboundedSender<Update>) {
    tokio::spawn(async move {
        let update = run_connection_probe(host, connection_id).await;
        crate::session::notify_ui(&updates, update);
    });
}

pub(crate) fn load_providers(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "プロバイダ一覧の取得",
        async move {
            let response = host.settings.list_providers(w::Empty::default()).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |providers| Update::Providers(providers.items.into_iter().map(Into::into).collect()),
    );
}

pub(crate) fn load_models(rpc: &RpcDispatcher, epoch: u64, connection_ids: Vec<String>) {
    let Some(host) = rpc.host() else {
        return;
    };
    for id in connection_ids {
        let request_id = id.clone();
        let host = host.clone();
        rpc.spawn(
            epoch,
            "モデル一覧の取得",
            async move {
                let response = host
                    .settings
                    .list_models(w::ModelsRequest {
                        id: request_id,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |models| Update::Models {
                connection_id: id,
                models: models.items.into_iter().map(Into::into).collect(),
            },
        );
    }
}

pub(crate) fn update_settings(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    expected_revision: u64,
    inputs: Vec<ConnectionInput>,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let wire_inputs: Vec<w::ConnectionInput> = inputs.into_iter().map(Into::into).collect();
    rpc.spawn(
        epoch,
        context,
        async move {
            let response = host
                .settings
                .update_settings(w::UpdateSettingsRequest {
                    expected_revision,
                    connections: wire_inputs,
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        settings_update,
    );
}

pub(crate) fn update_settings_and_probe(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    expected_revision: u64,
    inputs: Vec<ConnectionInput>,
    probe_connection_id: String,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let wire_inputs: Vec<w::ConnectionInput> = inputs.into_iter().map(Into::into).collect();
    let updates = rpc.updates();
    let probe_host = host.clone();
    rpc.spawn(
        epoch,
        context,
        async move {
            let response = host
                .settings
                .update_settings(w::UpdateSettingsRequest {
                    expected_revision,
                    connections: wire_inputs,
                    ..Default::default()
                })
                .await?;
            let updated = response.view().to_owned_message().map_err(decode)?;
            spawn_connection_probe(probe_host, probe_connection_id, updates);
            Ok(updated)
        },
        settings_update,
    );
}

pub(crate) fn delete_connection(rpc: &RpcDispatcher, epoch: u64, id: String, revision: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "接続の削除",
        async move {
            let response = host
                .settings
                .delete_connection(w::DeleteConnectionRequest {
                    id,
                    expected_revision: revision,
                    ..Default::default()
                })
                .await?;
            response.view().to_owned_message().map_err(decode)
        },
        settings_update,
    );
}

pub(crate) fn probe_connection(rpc: &RpcDispatcher, epoch: u64, connection_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn_update(epoch, run_connection_probe(host, connection_id));
}

pub(crate) fn register_secret(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    secret: String,
    probe_after: bool,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let id = connection_id.clone();
    let updates = rpc.updates();
    let probe_host = host.clone();
    rpc.spawn(
        epoch,
        "資格情報の登録",
        async move {
            let response = host
                .settings
                .register_credentials(w::CredentialRequest {
                    id: connection_id.clone(),
                    secret: Some(secret),
                    ..Default::default()
                })
                .await?;
            let out = response.view().to_owned_message().map_err(decode)?;
            if probe_after {
                spawn_connection_probe(probe_host, connection_id, updates);
            }
            Ok(out)
        },
        move |_| Update::CredentialRegistered { connection_id: id },
    );
}

pub(crate) fn register_secret_for_connections(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_ids: Vec<String>,
    secret: String,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "複数接続への資格情報登録",
        async move {
            for id in connection_ids {
                host.settings
                    .register_credentials(w::CredentialRequest {
                        id,
                        secret: Some(secret.clone()),
                        ..Default::default()
                    })
                    .await?;
            }
            Ok::<_, connectrpc::ConnectError>(())
        },
        |_| Update::Resnapshot,
    );
}

pub(crate) fn revoke_secret(rpc: &RpcDispatcher, epoch: u64, connection_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "資格情報の削除",
        async move {
            host.settings
                .revoke_credentials(w::Id {
                    id: connection_id,
                    ..Default::default()
                })
                .await
        },
        |_| Update::Resnapshot,
    );
}

pub(crate) fn save_search_provider_key(
    rpc: &RpcDispatcher,
    epoch: u64,
    expected_revision: u64,
    inputs: Vec<ConnectionInput>,
    target_id: String,
    secret: String,
) {
    let Some(host) = rpc.host() else {
        return;
    };
    let updates = rpc.updates();
    let return_id = target_id.clone();
    let wire_inputs: Vec<w::ConnectionInput> = inputs.into_iter().map(Into::into).collect();
    rpc.spawn(
        epoch,
        "Web 検索APIキーの登録",
        async move {
            if !wire_inputs.is_empty() {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision,
                        connections: wire_inputs,
                        ..Default::default()
                    })
                    .await?;
                let updated = response.view().to_owned_message().map_err(decode)?;
                crate::session::notify_ui(&updates, settings_update(updated));
            }
            host.settings
                .register_credentials(w::CredentialRequest {
                    id: target_id.clone(),
                    secret: Some(secret),
                    ..Default::default()
                })
                .await?;
            spawn_connection_probe(host, target_id, updates);
            Ok::<_, connectrpc::ConnectError>(())
        },
        move |_| Update::CredentialRegistered {
            connection_id: return_id,
        },
    );
}

pub(crate) fn quick_connect_llm(rpc: &RpcDispatcher, epoch: u64, req: QuickConnectRequest) {
    let Some(host) = rpc.host() else {
        return;
    };
    let wire_input: w::ConnectionInput = req.input.into();
    rpc.spawn_update(epoch, async move {
        let outcome: Result<Update, connectrpc::ConnectError> = async {
            if req.create {
                host.settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: req.revision,
                        connections: vec![wire_input],
                        ..Default::default()
                    })
                    .await?;
            }
            if !req.secret.is_empty() {
                host.settings
                    .register_credentials(w::CredentialRequest {
                        id: req.id.clone(),
                        secret: Some(req.secret),
                        ..Default::default()
                    })
                    .await?;
            }
            let probe = host
                .settings
                .check_connection(w::CheckRequest {
                    id: req.id.clone(),
                    request_key: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                })
                .await?
                .view()
                .to_owned_message()
                .map_err(decode)?;
            let check = probe.check.into_option();
            let healthy = check.as_ref().is_some_and(|c| c.healthy);
            let models = check.as_ref().map_or_default(|c| c.resources.clone());
            let error = check
                .as_ref()
                .and_then(|c| c.failure.as_option())
                .map(|f| f.message.clone())
                .filter(|msg| !msg.trim().is_empty());
            Ok(Update::SetupFinished {
                ok: healthy,
                connection_id: Some(req.id),
                models,
                error,
            })
        }
        .await;
        match outcome {
            Ok(update) => update,
            Err(err) => Update::SetupFinished {
                ok: false,
                connection_id: None,
                models: Vec::new(),
                error: Some(err.to_string()),
            },
        }
    });
}

pub(crate) fn complete_setup_selection(rpc: &RpcDispatcher, epoch: u64, selection: SetupSelection) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "初期設定の完了",
        async move {
            if let Some(conn) = selection.conn {
                let wire_input: w::ConnectionInput =
                    build_connection_model_input(&conn, &selection.model_id).into();
                let _written = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: selection.settings_revision,
                        connections: vec![wire_input],
                        ..Default::default()
                    })
                    .await?;
            }
            let patch = assignment_patch_set(ModelAssignment {
                connection_id: selection.connection_id.clone(),
                model_id: selection.model_id.clone(),
            });
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
                let updated = response.view().to_owned_message().map_err(decode)?;
                Ok(Update::Notification(Box::new(HostNotification::companion(
                    updated.into(),
                ))))
            } else {
                let assignments = w::ModelAssignments {
                    companion: Some(w::ModelAssignment {
                        connection_id: selection.connection_id.clone(),
                        model_id: selection.model_id.clone(),
                        ..Default::default()
                    })
                    .into(),
                    task: Some(w::ModelAssignment {
                        connection_id: selection.connection_id,
                        model_id: selection.model_id,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                };
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
                let created = response.view().to_owned_message().map_err(decode)?;
                Ok(Update::Created(created.id))
            }
        },
        |update| update,
    );
}
