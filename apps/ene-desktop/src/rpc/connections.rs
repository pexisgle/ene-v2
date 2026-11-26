//! Connection CRUD, probing, credential registration, and onboarding setup RPCs.

use super::{
    RpcDispatcher, companion_update, companions::shared_model_assignments, failure, id_request,
    settings_update,
};
use crate::{host::Host, update::Update};
use ene_client_core::{
    connections::build_connection_model_input,
    domain::{Companion, Connection, ConnectionInput, ModelAssignment},
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
        .check_connection(check_request(connection_id.clone()))
        .await
    {
        Ok(resp) => Update::Checked {
            connection_id,
            check: resp
                .into_owned()
                .check
                .into_option()
                .unwrap_or_default()
                .into(),
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

load! {
    load_providers: settings.list_providers("プロバイダ一覧の取得") =>
        |providers| Update::Providers(providers.items.into_iter().map(Into::into).collect());
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
                models: models
                    .into_owned()
                    .items
                    .into_iter()
                    .map(Into::into)
                    .collect(),
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
) {
    let request = settings_request(expected_revision, inputs);
    let updates = rpc.updates();
    rpc.call_owned(
        epoch,
        context,
        |host| async move {
            let response = host.settings.update_settings(request).await?;
            if let Some(id) = probe {
                spawn_connection_probe(host, id, updates);
            }
            Ok(response)
        },
        settings_update,
    );
}

fn settings_request(revision: u64, inputs: Vec<ConnectionInput>) -> w::UpdateSettingsRequest {
    w::UpdateSettingsRequest {
        expected_revision: revision,
        connections: inputs.into_iter().map(Into::into).collect(),
        ..Default::default()
    }
}

fn credential_request(id: String, secret: String) -> w::CredentialRequest {
    w::CredentialRequest {
        id,
        secret: Some(secret),
        ..Default::default()
    }
}

fn check_request(id: String) -> w::CheckRequest {
    w::CheckRequest {
        id,
        request_key: uuid::Uuid::new_v4().to_string(),
        ..Default::default()
    }
}

pub(crate) fn delete_connection(rpc: &RpcDispatcher, epoch: u64, id: String, revision: u64) {
    rpc.call_owned(
        epoch,
        "接続の削除",
        request!(settings.delete_connection(w::DeleteConnectionRequest {
            id,
            expected_revision: revision,
            ..Default::default()
        })),
        settings_update,
    );
}

pub(crate) fn probe_connection(rpc: &RpcDispatcher, epoch: u64, connection_id: String) {
    rpc.call_update(epoch, |host| run_connection_probe(host, connection_id));
}

pub(crate) fn register_secret(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_id: String,
    secret: String,
    probe_after: bool,
    settings: Option<(u64, Vec<ConnectionInput>)>,
) {
    let updates = rpc.updates();
    let id = connection_id.clone();
    let context = if settings.is_some() {
        "Web 検索APIキーの登録"
    } else {
        "資格情報の登録"
    };
    rpc.call(
        epoch,
        context,
        |host| async move {
            if let Some((revision, inputs)) = settings.filter(|(_, inputs)| !inputs.is_empty()) {
                let response = host
                    .settings
                    .update_settings(settings_request(revision, inputs))
                    .await?;
                crate::session::notify_ui(&updates, settings_update(response.into_owned()));
            }
            host.settings
                .register_credentials(credential_request(connection_id.clone(), secret))
                .await?;
            if probe_after {
                spawn_connection_probe(host, connection_id, updates);
            }
            Ok(())
        },
        move |()| Update::CredentialRegistered { connection_id: id },
    );
}

pub(crate) fn register_secret_for_connections(
    rpc: &RpcDispatcher,
    epoch: u64,
    connection_ids: Vec<String>,
    secret: String,
) {
    rpc.call(
        epoch,
        "複数接続への資格情報登録",
        |host| async move {
            for id in connection_ids {
                host.settings
                    .register_credentials(credential_request(id, secret.clone()))
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

pub(crate) fn quick_connect_llm(rpc: &RpcDispatcher, epoch: u64, req: QuickConnectRequest) {
    rpc.call_update(epoch, |host| async move {
        let outcome: Result<Update, connectrpc::ConnectError> = async {
            if req.create {
                host.settings
                    .update_settings(settings_request(req.revision, vec![req.input]))
                    .await?;
            }
            if !req.secret.is_empty() {
                host.settings
                    .register_credentials(credential_request(req.id.clone(), req.secret))
                    .await?;
            }
            let probe = host
                .settings
                .check_connection(check_request(req.id.clone()))
                .await?
                .into_owned();
            let check = probe.check.into_option().unwrap_or_default();
            Ok(Update::SetupFinished {
                ok: check.healthy,
                connection_id: Some(req.id),
                models: check.resources,
                error: check
                    .failure
                    .into_option()
                    .map(|failure| failure.message)
                    .filter(|message| !message.trim().is_empty()),
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
