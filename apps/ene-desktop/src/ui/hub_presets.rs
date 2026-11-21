//! Connection presets and first-run quick setup helpers on [`Hub`].

use super::{
    GOOGLE_AGENT_PLATFORM, Hub, ModelRoleSlot, SAKURA_ENDPOINT, SearchConnectionSpec, decode,
    hub_connections::spawn_connection_probe, llm_endpoint, llm_provider_input, search_adapter_of,
    search_connection_input, settings_update, workflows,
};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::Context;

/// Parameters for [`Hub::add_search_preset`].
pub(crate) struct SearchPresetSpec<'a> {
    pub(crate) preset_id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) adapter: &'a str,
    pub(crate) endpoint: &'a str,
    pub(crate) activate: bool,
}

struct SttPreset<'a> {
    fixed_id: Option<&'a str>,
    adapter: &'a str,
    name: String,
    endpoint: String,
    model: String,
    language: String,
}

struct EmbeddingPresetSpec<'a> {
    id: &'a str,
    name: &'a str,
    adapter: &'a str,
    endpoint: &'a str,
    model_id: &'a str,
    dimensions: u32,
}

fn search_provider_defaults(adapter: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match adapter {
        "exa" => Some(("search-exa", "Web検索 (Exa)", "https://api.exa.ai/search")),
        "tavily" => Some((
            "search-tavily",
            "Web検索 (Tavily)",
            "https://api.tavily.com/search",
        )),
        "brave" => Some((
            "search-brave",
            "Web検索 (Brave)",
            "https://api.search.brave.com/res/v1/web/search",
        )),
        _ => None,
    }
}

fn build_search_preset_inputs(
    connections: &[w::Connection],
    spec: &SearchPresetSpec<'_>,
    update_existing_always: bool,
) -> (String, Vec<w::ConnectionInput>) {
    let existing = connections.iter().find(|conn| {
        conn.id == spec.preset_id
            || search_adapter_of(conn)
                .is_some_and(|existing| existing.eq_ignore_ascii_case(spec.adapter))
    });
    let target_id = existing
        .map(|conn| conn.id.clone())
        .unwrap_or_else(|| spec.preset_id.to_owned());
    let has_enabled_other = connections
        .iter()
        .any(|conn| conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id);
    let should_enable = spec.activate || !has_enabled_other || existing.is_some_and(|c| c.enabled);
    let mut inputs = Vec::new();
    if let Some(conn) = existing {
        if update_existing_always || conn.enabled != should_enable {
            inputs.push(w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name.clone(),
                enabled: should_enable,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: conn.config.as_option().cloned().into(),
                ..Default::default()
            });
        }
    } else {
        inputs.push(search_connection_input(SearchConnectionSpec {
            id: target_id.clone(),
            name: spec.name.to_owned(),
            adapter: spec.adapter.to_owned(),
            endpoint: spec.endpoint.to_owned(),
            enabled: should_enable,
            expected_revision: 0,
            max_results: 5,
        }));
    }
    if spec.activate {
        for conn in connections {
            if conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id {
                inputs.push(w::ConnectionInput {
                    id: conn.id.clone(),
                    name: conn.name.clone(),
                    enabled: false,
                    expected_revision: conn.revision,
                    kind: conn.kind,
                    config: conn.config.as_option().cloned().into(),
                    ..Default::default()
                });
            }
        }
    }
    (target_id, inputs)
}

impl Hub {
    pub(crate) fn assign_search_connection(
        &mut self,
        connection_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs: Vec<w::ConnectionInput> = settings
            .connections
            .iter()
            .filter(|conn| conn.kind == w::ConnectionKind::Search)
            .map(|conn| w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name.clone(),
                enabled: connection_id.as_deref() == Some(conn.id.as_str()),
                expected_revision: conn.revision,
                kind: conn.kind,
                config: conn.config.as_option().cloned().into(),
                ..Default::default()
            })
            .collect();
        if inputs.is_empty() {
            return;
        }
        self.request(
            "Web 検索接続の割当",
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

    pub(crate) fn add_search_preset(
        &mut self,
        spec: SearchPresetSpec<'_>,
        cx: &mut Context<Self>,
    ) -> String {
        let Some(settings) = self.model.settings().cloned() else {
            return spec.preset_id.to_owned();
        };
        let Some(host) = self.host() else {
            return spec.preset_id.to_owned();
        };
        let (target_id, inputs) = build_search_preset_inputs(&settings.connections, &spec, true);
        self.request(
            "Web 検索接続の追加",
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
        target_id
    }

    pub(crate) fn save_search_provider_key(
        &mut self,
        adapter: &'static str,
        secret: String,
        activate: bool,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        let Some((preset_id, name, endpoint)) = search_provider_defaults(adapter) else {
            return;
        };
        if secret.is_empty() {
            return;
        }
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let spec = SearchPresetSpec {
            preset_id,
            name,
            adapter,
            endpoint,
            activate,
        };
        let (target_id, inputs) = build_search_preset_inputs(&settings.connections, &spec, false);
        self.checking_connections.insert(target_id.clone());
        let updates = self.updates.clone();
        let return_id = target_id.clone();
        let settings_revision = settings.revision;
        self.request(
            "Web 検索APIキーの登録",
            async move {
                if !inputs.is_empty() {
                    let response = host
                        .settings
                        .update_settings(w::UpdateSettingsRequest {
                            expected_revision: settings_revision,
                            connections: inputs,
                            ..Default::default()
                        })
                        .await?;
                    let updated = response.view().to_owned_message().map_err(decode)?;
                    let _ = updates.send(settings_update(updated));
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
            move |_| super::Update::CredentialRegistered {
                connection_id: return_id,
            },
        );
        cx.notify();
    }

    pub(crate) fn add_llm_from_provider(
        &mut self,
        provider: w::LlmProvider,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.upsert_connection(
            "接続の追加",
            llm_provider_input(uuid::Uuid::new_v4().to_string(), provider, model_ids),
            cx,
        );
    }

    pub(crate) fn quick_connect_llm(
        &mut self,
        provider: w::LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let Some(host) = self.host() else { return };
        if self.setup_pending {
            return;
        }
        // Reuse a failed attempt only at the exact catalog endpoint.
        let existing = settings.connections.iter().find(|c| {
            c.enabled
                && llm_endpoint(c).is_some_and(|ep| {
                    ep.adapter == provider.adapter
                        && ep.endpoint == provider.endpoint
                        && (ep.adapter != GOOGLE_AGENT_PLATFORM
                            || serde_json::from_str::<serde_json::Value>(&ep.options)
                                .ok()
                                .and_then(|options| {
                                    options["project_id"]
                                        .as_str()
                                        .map(|id| !id.trim().is_empty())
                                })
                                != Some(true))
                })
        });
        let id = existing
            .map(|c| c.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let revision = settings.revision;
        let create = existing.is_none();
        let input = llm_provider_input(id.clone(), provider, Vec::new());
        self.setup_pending = true;
        self.setup_error = None;
        self.setup_error_detail = None;
        self.request(
            "AI接続",
            async move {
                Ok(workflows::quick_connect_llm(host, revision, id, create, input, secret).await)
            },
            |outcome| outcome,
        );
        cx.notify();
    }

    pub(crate) fn complete_setup_selection(
        &mut self,
        connection_id: String,
        model_id: String,
        companion_name: Option<String>,
        voice_identifier: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let model_id = model_id.trim().to_owned();
        if model_id.is_empty() {
            return;
        }
        let conn = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned();
        let existing_comp = self.model.companion().cloned();
        let voice_clean = voice_identifier
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let name_clean = companion_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "ene".to_owned());

        self.pending_setup_connection_id = None;
        self.pending_setup_models.clear();

        self.request(
            "初期設定の完了",
            workflows::complete_setup_selection(
                host,
                workflows::SetupSelection {
                    settings_revision: settings.revision,
                    conn,
                    connection_id,
                    model_id,
                    existing_comp,
                    companion_name: name_clean,
                    voice_identifier: voice_clean,
                },
            ),
            |update| update,
        );
        cx.notify();
    }

    pub(crate) fn add_voicevox_preset(
        &mut self,
        speaker: u32,
        speed: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let existing_rev = settings
            .connections
            .iter()
            .find(|c| c.id == "voicevox")
            .map(|c| c.revision)
            .unwrap_or(0);
        let options = match speed {
            Some(speed) => format!("{{\"speaker\":{speaker},\"speed\":{speed:.2}}}"),
            None => format!("{{\"speaker\":{speaker}}}"),
        };
        let input = w::ConnectionInput {
            id: "voicevox".into(),
            name: "VOICEVOX（ローカル）".into(),
            enabled: true,
            expected_revision: existing_rev,
            kind: w::ConnectionKind::Tts.into(),
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Tts(Box::new(
                    w::TtsConnection {
                        endpoint: Some(w::ConnectionEndpoint {
                            adapter: "voicevox".into(),
                            endpoint: "http://127.0.0.1:50021".into(),
                            options_schema: String::new(),
                            options,
                            ..Default::default()
                        })
                        .into(),
                        voice_ids: vec![speaker.to_string()],
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        self.checking_connections.insert("voicevox".into());
        let updates = self.updates.clone();
        let probe_host = host.clone();
        self.request(
            "VOICEVOX接続の追加",
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
                spawn_connection_probe(probe_host, "voicevox".into(), updates);
                Ok(updated)
            },
            settings_update,
        );
        cx.notify();
    }

    pub(crate) fn add_openai_tts_preset(
        &mut self,
        endpoint: String,
        model: String,
        voice: String,
        cx: &mut Context<Self>,
    ) {
        let options = serde_json::json!({
            "protocol": "openai",
            "model": if model.trim().is_empty() { "tts-1" } else { model.trim() },
            "voice": if voice.trim().is_empty() { "alloy" } else { voice.trim() },
        })
        .to_string();
        self.upsert_connection(
            "TTS接続の追加",
            w::ConnectionInput {
                id: uuid::Uuid::new_v4().to_string(),
                name: "OpenAI互換 TTS".into(),
                enabled: true,
                expected_revision: 0,
                kind: w::ConnectionKind::Tts.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Tts(Box::new(
                        w::TtsConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "openai".into(),
                                endpoint: if endpoint.trim().is_empty() {
                                    "https://api.openai.com/v1".into()
                                } else {
                                    endpoint.trim().to_owned()
                                },
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            voice_ids: vec![if voice.trim().is_empty() {
                                "alloy".into()
                            } else {
                                voice.trim().to_owned()
                            }],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_stt_preset(
        &mut self,
        name: String,
        endpoint: String,
        model: String,
        language: String,
        cx: &mut Context<Self>,
    ) {
        self.add_stt_preset_with_spec(
            SttPreset {
                fixed_id: None,
                adapter: "openai",
                name,
                endpoint,
                model,
                language,
            },
            cx,
        );
    }

    fn add_stt_preset_with_spec(&mut self, spec: SttPreset<'_>, cx: &mut Context<Self>) {
        let existing_rev = spec
            .fixed_id
            .and_then(|fid| {
                self.model.settings().and_then(|s| {
                    s.connections
                        .iter()
                        .find(|c| c.id == fid)
                        .map(|c| c.revision)
                })
            })
            .unwrap_or(0);
        let protocol = if spec.adapter == "sakura" {
            "sakura"
        } else {
            "openai"
        };
        let options = serde_json::json!({
            "protocol": protocol,
            "model": if spec.model.trim().is_empty() { "whisper-1" } else { spec.model.trim() },
            "language": if spec.language.trim().is_empty() { "ja" } else { spec.language.trim() },
        })
        .to_string();
        let lang = if spec.language.trim().is_empty() {
            "ja".to_owned()
        } else {
            spec.language.trim().to_owned()
        };
        self.upsert_connection(
            "音声認識（STT）接続の追加",
            w::ConnectionInput {
                id: spec
                    .fixed_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: spec.name,
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Stt.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Stt(Box::new(
                        w::SttConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: spec.adapter.to_owned(),
                                endpoint: spec.endpoint,
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            language_hints: vec![lang],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_sakura_tts_preset(&mut self, cx: &mut Context<Self>) {
        let existing_rev = self
            .model
            .settings()
            .and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == "sakura-tts")
                    .map(|c| c.revision)
            })
            .unwrap_or(0);
        let options = serde_json::json!({
            "protocol": "sakura",
            "model": "zundamon",
            "voice": "normal",
            "response_format": "wav",
        })
        .to_string();
        self.upsert_connection(
            "さくらのAI Engine TTS接続の追加",
            w::ConnectionInput {
                id: "sakura-tts".into(),
                name: "さくらのAI Engine TTS".into(),
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Tts.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Tts(Box::new(
                        w::TtsConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "sakura".into(),
                                endpoint: SAKURA_ENDPOINT.into(),
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            voice_ids: vec!["zundamon/normal".into()],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_sakura_stt_preset(&mut self, cx: &mut Context<Self>) {
        self.add_stt_preset_with_spec(
            SttPreset {
                fixed_id: Some("sakura-stt"),
                adapter: "sakura",
                name: "さくらのAI Engine STT".into(),
                endpoint: SAKURA_ENDPOINT.into(),
                model: "whisper-large-v3-turbo".into(),
                language: "ja".into(),
            },
            cx,
        );
    }

    pub(crate) fn add_sakura_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            EmbeddingPresetSpec {
                id: "sakura-embedding",
                name: "さくらのAI Engine Embedding",
                adapter: "sakura",
                endpoint: SAKURA_ENDPOINT,
                model_id: "multilingual-e5-large",
                dimensions: 1024,
            },
            cx,
        );
    }

    pub(crate) fn add_openai_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            EmbeddingPresetSpec {
                id: "openai-embedding",
                name: "OpenAI Embedding",
                adapter: "openai",
                endpoint: "https://api.openai.com/v1",
                model_id: "text-embedding-3-small",
                dimensions: 1536,
            },
            cx,
        );
    }

    fn add_embedding_preset(&mut self, spec: EmbeddingPresetSpec<'_>, cx: &mut Context<Self>) {
        let existing_rev = self
            .model
            .settings()
            .and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == spec.id)
                    .map(|c| c.revision)
            })
            .unwrap_or(0);
        let should_assign = self.model.companion().is_some_and(|c| {
            c.assignments
                .as_option()
                .and_then(|a| a.embedding.as_option())
                .is_none_or(|e| e.connection_id.trim().is_empty())
        });
        self.upsert_connection(
            "埋め込み（Embedding）接続の追加",
            w::ConnectionInput {
                id: spec.id.into(),
                name: spec.name.into(),
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Embedding.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Embedding(Box::new(
                        w::EmbeddingConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: spec.adapter.into(),
                                endpoint: spec.endpoint.into(),
                                options_schema: String::new(),
                                options: "{}".into(),
                                ..Default::default()
                            })
                            .into(),
                            model_id: spec.model_id.into(),
                            dimensions: spec.dimensions,
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
        if should_assign {
            self.assign_model(
                ModelRoleSlot::Embedding,
                Some((spec.id.into(), spec.model_id.into())),
                cx,
            );
        }
    }
}
