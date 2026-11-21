//! Connection CRUD, model/voice selection, probing, and credential management on [`Hub`].

use super::{
    Hub, ModelRoleSlot, apply_stt_model_to_config, apply_tts_voice_to_config,
    build_stt_assignment_inputs, build_tts_assignment_inputs, companion_update, decode,
    default_embedding_dimensions, is_local_voicevox_conn, is_sakura_conn, settings_update,
    workflows,
};
use crate::{host::Host, update::Update};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::Context;
use tokio::sync::mpsc::UnboundedSender;

pub(super) fn spawn_connection_probe(
    host: Host,
    connection_id: String,
    updates: UnboundedSender<Update>,
) {
    tokio::spawn(async move {
        let update = workflows::probe_connection(host, connection_id).await;
        let _ = updates.send(update);
    });
}

impl Hub {
    pub(crate) fn load_providers(&mut self, _cx: &mut Context<Self>) {
        if self.model.connections.providers.is_some() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プロバイダ一覧の取得",
            async move {
                let response = host.settings.list_providers(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |providers| Update::Providers(providers.items),
        );
    }

    pub(crate) fn load_models(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        for connection in settings
            .connections
            .iter()
            .filter(|connection| connection.kind == w::ConnectionKind::Llm)
        {
            let id = connection.id.clone();
            let request_id = id.clone();
            let host = host.clone();
            self.request(
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
                    models: models.items,
                },
            );
        }
    }

    pub(crate) fn upsert_connection(
        &mut self,
        context: &'static str,
        input: w::ConnectionInput,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            context,
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![input],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            settings_update,
        );
        cx.notify();
    }

    pub(crate) fn delete_connection(&mut self, id: String, revision: u64, cx: &mut Context<Self>) {
        let Some(host) = self.host() else { return };
        self.request(
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
        cx.notify();
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: w::Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let is_voicevox = is_local_voicevox_conn(&connection);
        let conn_id = connection.id.clone();
        let input = w::ConnectionInput {
            id: connection.id,
            name: connection.name,
            enabled,
            expected_revision: connection.revision,
            kind: connection.kind,
            config: connection.config,
            ..Default::default()
        };
        if !(enabled && is_voicevox) {
            self.upsert_connection("接続状態の更新", input, cx);
            return;
        }
        self.checking_connections.insert(conn_id.clone());
        let updates = self.updates.clone();
        let runtime_host = host.clone();
        self.request(
            "接続状態の更新",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![input],
                        ..Default::default()
                    })
                    .await?;
                let updated = response.view().to_owned_message().map_err(decode)?;
                spawn_connection_probe(runtime_host, conn_id, updates);
                Ok(updated)
            },
            settings_update,
        );
        cx.notify();
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        if self.checking_connections.contains(&connection_id) {
            return;
        }
        self.checking_connections.insert(connection_id.clone());
        self.request(
            "接続確認",
            async move { Ok(workflows::probe_connection(host, connection_id).await) },
            |update| update,
        );
        cx.notify();
    }

    pub(crate) fn select_connection_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_id = model_id.trim().to_owned();
        if model_id.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        if let Some(w::connection_config::Config::Llm(llm)) = cfg.config.as_mut() {
            llm.model_ids = vec![model_id.clone()];
        }
        self.upsert_connection(
            "使用モデルの変更",
            w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
        if let Some(comp) = self.model.companion().cloned() {
            let assignments = comp.assignments.as_option();
            let comp_uses_conn = assignments
                .and_then(|a| a.companion.as_option())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            let task_uses_conn = assignments
                .and_then(|a| a.task.as_option())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            if comp_uses_conn {
                self.assign_model(
                    ModelRoleSlot::Companion,
                    Some((connection_id.clone(), model_id.clone())),
                    cx,
                );
            }
            if task_uses_conn {
                self.assign_model(ModelRoleSlot::Task, Some((connection_id, model_id)), cx);
            }
        }
    }

    pub(crate) fn select_voicevox_speaker(
        &mut self,
        connection_id: String,
        speaker_id: String,
        cx: &mut Context<Self>,
    ) {
        self.select_tts_voice(connection_id, speaker_id, cx);
    }

    pub(crate) fn select_tts_voice(
        &mut self,
        connection_id: String,
        voice_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let voice_clean = voice_id.trim().to_owned();
        if voice_clean.is_empty() {
            return;
        }
        let is_sakura = is_sakura_conn(&conn);
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        apply_tts_voice_to_config(&mut cfg, is_sakura, &voice_clean);
        self.upsert_connection(
            "音声話者の変更",
            w::ConnectionInput {
                id: conn.id,
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn select_stt_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        apply_stt_model_to_config(&mut cfg, &model_clean);
        self.upsert_connection(
            "音声認識モデルの変更",
            w::ConnectionInput {
                id: conn.id,
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
    }

    /// Assigns the active STT connection and model atomically across all STT connections.
    pub(crate) fn assign_stt_model(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs = build_stt_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, mid)| (cid.as_str(), mid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        self.request(
            "音声認識モデルの割当",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            settings_update,
        );
        cx.notify();
    }

    /// Assigns the active TTS connection and voice atomically across all TTS connections,
    /// probing local VOICEVOX when newly enabled and syncing companion `voice_identifier`.
    pub(crate) fn assign_tts_voice(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs = build_tts_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, vid)| (cid.as_str(), vid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        let target_voice_clean = selection
            .as_ref()
            .map(|(_, vid)| vid.trim().to_owned())
            .filter(|v| !v.is_empty());
        let probe_voicevox_id = selection.as_ref().and_then(|(cid, _)| {
            let conn = settings.connections.iter().find(|c| c.id == *cid)?;
            (is_local_voicevox_conn(conn)
                && (!conn.enabled || self.voice_choices_for_connection(&conn.id).len() <= 1))
                .then(|| conn.id.clone())
        });
        if let Some(ref check_id) = probe_voicevox_id {
            self.checking_connections.insert(check_id.clone());
        }
        let check_updates = self.updates.clone();
        let probe_host = host.clone();
        let settings_host = host.clone();
        self.request(
            "音声合成（TTS）の割当",
            async move {
                let response = settings_host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
                        ..Default::default()
                    })
                    .await?;
                let updated = response.view().to_owned_message().map_err(decode)?;
                if let Some(check_id) = probe_voicevox_id {
                    spawn_connection_probe(probe_host, check_id, check_updates);
                }
                Ok(updated)
            },
            settings_update,
        );
        self.sync_companion_voice(host, target_voice_clean);
        cx.notify();
    }

    fn sync_companion_voice(&self, host: Host, voice: Option<String>) {
        let Some(voice_clean) = voice else {
            return;
        };
        let Some(comp) = self.model.companion().cloned() else {
            return;
        };
        if comp.voice_identifier.as_deref() == Some(voice_clean.as_str()) {
            return;
        }
        self.request(
            "コンパニオン話者の同期",
            async move {
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: comp.id,
                        revision: comp.revision,
                        voice_identifier: Some(voice_clean),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            companion_update,
        );
    }

    pub(crate) fn select_embedding_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        if let Some(w::connection_config::Config::Embedding(emb)) = cfg.config.as_mut() {
            emb.model_id = model_clean.clone();
            emb.dimensions = default_embedding_dimensions(&model_clean);
        }
        self.upsert_connection(
            "埋め込みモデルの変更",
            w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
        if let Some(comp) = self.model.companion()
            && comp
                .assignments
                .as_option()
                .and_then(|a| a.embedding.as_option())
                .is_some_and(|a| a.connection_id == connection_id)
        {
            self.assign_model(
                ModelRoleSlot::Embedding,
                Some((connection_id, model_clean)),
                cx,
            );
        }
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: String,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let is_search = self.model.settings().is_some_and(|s| {
            s.connections
                .iter()
                .any(|c| c.id == connection_id && c.kind == w::ConnectionKind::Search)
        });
        if is_search {
            self.checking_connections.insert(connection_id.clone());
        }
        let id = connection_id.clone();
        let updates = self.updates.clone();
        self.request(
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
                if is_search {
                    spawn_connection_probe(host, connection_id, updates);
                }
                Ok(out)
            },
            move |_| Update::CredentialRegistered { connection_id: id },
        );
        cx.notify();
    }

    pub(crate) fn register_secret_for_connections(
        &mut self,
        connection_ids: Vec<String>,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() || connection_ids.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
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
        cx.notify();
    }

    pub(crate) fn revoke_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
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
        cx.notify();
    }
}
