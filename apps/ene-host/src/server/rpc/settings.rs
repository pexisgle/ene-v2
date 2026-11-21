//! Settings, connections, credentials, policy and approval services.
use super::Services;
use crate::host::{
    db::Store,
    error::HostError,
    model,
    notify::Notifier,
    permission, settings,
    settings::{Connection, CredentialState},
    wire,
};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use ene_rpc as w;
use rig_core::catalog::ModelSpec;
use rig_ecs::prelude::App;

#[allow(refining_impl_trait)]
impl w::SettingsService for Services {
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        self.read(|app| Ok(wire::settings(&app.world().resource::<Store>().settings()?)))
            .await
    }

    async fn update_settings(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        let request = request.to_owned_message();
        let expected_revision = request.expected_revision;
        let mut inputs = Vec::with_capacity(request.connections.len());
        for input in request.connections {
            let expected = input.expected_revision;
            inputs.push((wire::connection_from(input)?, expected));
        }
        // Pre-start enabled plugins referenced in `inputs` so that when
        // `SettingsUpdated` fires on reload inside `self.call`, newly enabled
        // plugin adapters (e.g. openrouter-wasm) are already running.
        for (conn, _) in &inputs {
            if !conn.enabled {
                continue;
            }
            let adapter = conn.endpoint().adapter.clone();
            if self.runtime.providers().get(&adapter).is_none() {
                continue;
            }
            let extension = match conn.kind {
                crate::host::settings::ConnectionKind::Embedding => {
                    ene_plugin::Extension::Embedding
                }
                crate::host::settings::ConnectionKind::Llm => ene_plugin::Extension::Llm,
                crate::host::settings::ConnectionKind::Tts => ene_plugin::Extension::Tts,
                crate::host::settings::ConnectionKind::Stt => ene_plugin::Extension::Stt,
                crate::host::settings::ConnectionKind::Acp => ene_plugin::Extension::Harness,
                crate::host::settings::ConnectionKind::Mcp
                | crate::host::settings::ConnectionKind::Search => continue,
            };
            let _ = self.runtime.providers().start(&adapter, extension).await;
        }
        let view = self
            .call(move |app| {
                let store = app.world().resource::<Store>().clone();
                let settings = store.update_settings(expected_revision, &inputs)?;
                let view = wire::settings(&settings);
                notify_settings(app, view.clone());
                Ok(view)
            })
            .await?;
        // The save lands first. A plugin starts because a stored connection
        // names it. `start_used` logs a plugin that will not start and carries
        // on, the same policy as startup, so the save still succeeds.
        let store = self
            .runtime
            .read(|app| Ok(app.world().resource::<Store>().clone()))
            .await
            .map_err(ConnectError::from)?;
        self.runtime.providers().start_used(&store).await;
        if let Ok(current) = store.settings() {
            let providers = self.runtime.providers();
            let active_vv = current
                .connections
                .iter()
                .find(|conn| is_builtin_voicevox(conn, &providers));
            if let Some(vv) = active_vv {
                if !cfg!(test) {
                    let mgr = self.runtime.voicevox();
                    let base = vv.endpoint().endpoint.clone();
                    let options = vv.endpoint().options.clone();
                    tokio::spawn(async move {
                        if let Err(error) = mgr.ensure_running(&base, &options).await {
                            tracing::warn!(%error, "could not auto-start VOICEVOX engine after settings update");
                        }
                    });
                }
            } else {
                self.runtime.voicevox().stop().await;
            }
        }
        Ok(view)
    }

    async fn list_pending_approvals(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PendingApprovals> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PendingApprovals {
                items: store
                    .approvals(permission::ApprovalStatus::Pending)?
                    .iter()
                    .map(wire::approval)
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn delete_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::DeleteConnectionRequest>,
    ) -> ServiceResult<w::Settings> {
        let id = request.id.to_owned();
        let revision = request.expected_revision;
        let credentials = self.runtime.credentials();
        let response = self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let (settings, companions) = store.delete_connection(&id, revision, &credentials)?;
            let view = wire::settings(&settings);
            notify_settings(app, view.clone());
            for companion in companions {
                if let Err(error) = crate::host::agent::sync_companion(app.world_mut(), &companion.id) {
                    tracing::warn!(%error, "could not rebind companion after connection removal");
                }
                crate::host::mind::notify(app.world_mut(), &companion);
            }
            Ok(view)
        }).await?;
        let providers = self.runtime.providers();
        let has_voicevox = self
            .runtime
            .read(move |app| {
                Ok(app
                    .world()
                    .resource::<Store>()
                    .settings()?
                    .connections
                    .iter()
                    .any(|connection| is_builtin_voicevox(connection, &providers)))
            })
            .await;
        if matches!(has_voicevox, Ok(false)) {
            self.runtime.voicevox().stop().await;
        }
        Ok(response)
    }

    async fn submit_approval(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubmitApprovalRequest>,
    ) -> ServiceResult<w::SubmitApprovalResponse> {
        let decision = match request.decision.as_known() {
            Some(w::ApprovalDecision::Approve) => permission::ApprovalDecision::Approve,
            Some(w::ApprovalDecision::AlwaysApprove) => permission::ApprovalDecision::AlwaysApprove,
            Some(w::ApprovalDecision::Deny) => permission::ApprovalDecision::Deny,
            Some(w::ApprovalDecision::Unspecified) | None => {
                permission::ApprovalDecision::Unspecified
            }
        };
        let request_id = request.request_id.to_owned();
        self.call(move |app| {
            let outcome = permission::submit_with_outcome(app.world_mut(), &request_id, decision)?;
            Ok(w::SubmitApprovalResponse {
                acknowledged: true,
                status: wire::approval_status(outcome.approval.status).into(),
                rule_saved: outcome.rule_saved,
                rule_error: outcome.rule_error,
                ..Default::default()
            })
        })
        .await
    }

    async fn list_policy_rules(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyRules> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyRules {
                items: store
                    .policy_rules()?
                    .iter()
                    .map(wire::policy_rule)
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn update_policy_rules(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyRulesRequest>,
    ) -> ServiceResult<w::PolicyRules> {
        let request = request.to_owned_message();
        let mut rules = Vec::with_capacity(request.add_or_update.len());
        for rule in request.add_or_update {
            rules.push(rule_from(rule)?);
        }
        let remove_ids = request.remove_ids;
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let updated = store.apply_policy_rules(&rules, &remove_ids)?;
            let items: Vec<w::PolicyRule> = updated.iter().map(wire::policy_rule).collect();
            app.world_mut().resource_mut::<Notifier>().push(
                None,
                None,
                w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
                    rules: items.clone(),
                    ..Default::default()
                })),
            );
            Ok(w::PolicyRules {
                items,
                ..Default::default()
            })
        })
        .await
    }

    async fn list_policy_texts(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyTexts> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyTexts {
                items: store
                    .policy_texts()?
                    .iter()
                    .map(wire::policy_text)
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn update_policy_texts(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyTextsRequest>,
    ) -> ServiceResult<w::PolicyTexts> {
        let request = request.to_owned_message();
        let mut texts = Vec::with_capacity(request.add_or_update.len());
        for text in request.add_or_update {
            texts.push(wire::policy_text_from(&text));
        }
        let remove_ids = request.remove_ids;
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let updated = store.apply_policy_texts(&texts, &remove_ids)?;
            let items: Vec<w::PolicyText> = updated.iter().map(wire::policy_text).collect();
            app.world_mut().resource_mut::<Notifier>().push(
                None,
                None,
                w::notification::Event::PolicyTextsUpdated(Box::new(w::PolicyTextsUpdated {
                    texts: items.clone(),
                    ..Default::default()
                })),
            );
            Ok(w::PolicyTexts {
                items,
                ..Default::default()
            })
        })
        .await
    }

    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::LlmProviders> {
        Response::ok(w::LlmProviders {
            items: settings::providers(&self.runtime.providers()),
            ..Default::default()
        })
    }

    async fn list_models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        let id = request.id.to_owned();
        self.read(move |app| {
            let world = app.world();
            let connection = world
                .resource::<Store>()
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound(("connection not found").into()))?;
            let catalog = world.resource::<rig_ecs::model::Models>().0.catalog();
            Ok(w::Models {
                items: model::models(&connection, catalog)
                    .into_iter()
                    .map(|(id, spec)| model_item(id, &spec))
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn register_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let id = request.id.to_owned();
        let secret = request
            .secret
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| ConnectError::invalid_argument("secret is required"))?
            .to_owned();
        let credentials = self.runtime.credentials();
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            if store.connection(&id)?.is_none() {
                return Err(HostError::NotFound("connection not found".into()));
            }
            model::save_secret(&credentials, &id, &secret)?;
            store.set_credential_state(&id, CredentialState::Registered)?;
            notify_settings(app, wire::settings(&store.settings()?));
            Ok(w::CredentialResult {
                credential_state: CredentialState::Registered.as_str().into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn revoke_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        let credentials = self.runtime.credentials();
        let providers = self.runtime.providers();
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let connection = store
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
            model::delete_secret(&credentials, &id)?;
            let state = model::credential_state(&connection, &credentials, &providers)?;
            store.set_credential_state(&id, state)?;
            notify_settings(app, wire::settings(&store.settings()?));
            Ok(w::Empty::default())
        })
        .await
    }

    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn check_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        let id = request.id.to_owned();
        let (connection, store) = self
            .runtime
            .read(move |app| {
                let store = app.world().resource::<Store>().clone();
                let connection = store
                    .connection(&id)?
                    .ok_or_else(|| HostError::NotFound(("connection not found").into()))?;
                Ok((connection, store))
            })
            .await
            .map_err(ConnectError::from)?;
        let credentials = self.runtime.credentials();
        let agent_platform = connection.endpoint().adapter == model::agent_platform::ADAPTER;
        let is_mcp = connection.kind == settings::ConnectionKind::Mcp;
        let is_search = connection.kind == settings::ConnectionKind::Search;
        let checked_revision =
            (agent_platform || is_mcp || is_search).then_some(connection.revision);
        let checked_credentials = (agent_platform || is_search).then(|| credentials.generation());
        // The probe performs a network request, so it must not run on the ECS
        // thread; the connection was captured above.
        let check = if is_mcp {
            if !connection.enabled {
                crate::host::settings::Check {
                    healthy: false,
                    checked_at: crate::host::util::now(),
                    failure: Some(crate::host::error::Failure::new(
                        "connection_check",
                        "MCP 接続が無効化されています。",
                    )),
                    capabilities: Vec::new(),
                    resources: Vec::new(),
                }
            } else {
                let reconnect_id = connection.id.clone();
                self.call(move |app| {
                    crate::host::mcp::request_reconnect(app.world_mut(), &reconnect_id);
                    Ok(())
                })
                .await?;
                let uses_npx = matches!(&connection.config,
                    settings::ConnectionConfig::Mcp(config)
                        if crate::host::mcp::is_npx_command(&config.command));
                // npm may have to download a package before the server can list tools.
                let timeout = std::time::Duration::from_secs(if uses_npx { 60 } else { 20 });
                let outcome = tokio::time::timeout(timeout, async {
                    loop {
                        let id = connection.id.clone();
                        let revision = connection.revision;
                        let state = self
                            .runtime
                            .read(move |app| {
                                let world = app.world();
                                if !world
                                    .resource::<Store>()
                                    .connection(&id)?
                                    .is_some_and(|current| current.revision == revision)
                                {
                                    return Err(HostError::Conflict(
                                        "MCP connection changed during the check".into(),
                                    ));
                                }
                                Ok(crate::host::mcp::check_state(world, &id, revision))
                            })
                            .await?;
                        match state {
                            crate::host::mcp::CheckState::Connected(tools) => {
                                return Ok::<_, HostError>(Ok(tools));
                            }
                            crate::host::mcp::CheckState::Failed(reason) => {
                                return Ok::<_, HostError>(Err(reason));
                            }
                            crate::host::mcp::CheckState::Connecting => {}
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                })
                .await;
                let default_message = if uses_npx {
                    "MCP に接続できませんでした。ホストで Node.js と npm が使えること、起動コマンドと通信環境を確認してください。"
                } else {
                    "MCP に接続できませんでした。サーバー URL または起動コマンドを確認してください。"
                };
                match outcome {
                    Ok(Ok(Ok(tools))) => crate::host::settings::Check {
                        healthy: true,
                        checked_at: crate::host::util::now(),
                        failure: None,
                        capabilities: Vec::new(),
                        resources: tools,
                    },
                    Ok(Ok(Err(reason))) => crate::host::settings::Check {
                        healthy: false,
                        checked_at: crate::host::util::now(),
                        failure: Some(crate::host::error::Failure::new(
                            "connection_check",
                            format!("{default_message} ({reason})"),
                        )),
                        capabilities: Vec::new(),
                        resources: Vec::new(),
                    },
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => crate::host::settings::Check {
                        healthy: false,
                        checked_at: crate::host::util::now(),
                        failure: Some(crate::host::error::Failure::new(
                            "connection_check",
                            default_message,
                        )),
                        capabilities: Vec::new(),
                        resources: Vec::new(),
                    },
                }
            }
        } else if connection.kind == crate::host::settings::ConnectionKind::Tts
            && connection
                .endpoint()
                .adapter
                .eq_ignore_ascii_case("voicevox")
            && !model::is_sakura_endpoint(&connection.endpoint().endpoint)
            && self.runtime.providers().get("voicevox").is_none()
        {
            let voicevox = self.runtime.voicevox();
            probe_voicevox_tts(&connection, &voicevox).await
        } else if matches!(
            connection.kind,
            crate::host::settings::ConnectionKind::Stt | crate::host::settings::ConnectionKind::Tts
        ) {
            probe_openai_speech(&connection, &credentials, &self.runtime.providers()).await
        } else if connection.kind == crate::host::settings::ConnectionKind::Embedding {
            probe_embedding(&connection, &credentials, &self.runtime.providers()).await
        } else if connection.kind == crate::host::settings::ConnectionKind::Search {
            crate::host::web_search::probe(&connection, &credentials, &self.runtime.providers())
                .await
        } else {
            model::probe(&store, &credentials, &connection, &self.runtime.providers()).await
        };
        let request_id = request.request_key.to_owned();
        let resource_id = request.resource_id.map(str::to_owned);
        let id = connection.id;
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            if let Some(revision) = checked_revision
                && (checked_credentials
                    .is_some_and(|generation| credentials.generation() != generation)
                    || store
                        .connection(&id)?
                        .is_some_and(|current| current.revision != revision))
            {
                return Err(HostError::Conflict(
                    "connection changed during the check".into(),
                ));
            }
            store.set_check(&id, &check)?;
            if checked_revision.is_some() {
                notify_settings(app, wire::settings(&store.settings()?));
            }
            Ok(w::CheckResult {
                request_id,
                success: check.healthy,
                resource_id,
                check: Some(wire::check(&check)).into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn list_plugins(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PluginList> {
        let providers = self.runtime.providers();
        self.read(move |app| {
            let store = app.world().resource::<Store>();
            let entries = store.plugin_entries()?;
            let mut by_id: std::collections::BTreeMap<String, w::PluginInfo> = entries
                .into_iter()
                .map(|entry| {
                    let info = w::PluginInfo {
                        id: entry.plugin_id.clone(),
                        name: entry.name,
                        version: entry.version,
                        enabled: entry.enabled,
                        path: entry.path,
                        installed_at: entry.installed_at,
                        ..Default::default()
                    };
                    (entry.plugin_id, info)
                })
                .collect();
            for adapter in providers.adapters() {
                let Some(loaded) = providers.get(adapter) else {
                    continue;
                };
                let manifest = &loaded.package.manifest;
                let extensions: Vec<String> = manifest
                    .entrypoints
                    .iter()
                    .flat_map(|ep| ep.provides.iter())
                    .map(|ext| format!("{ext:?}").to_ascii_lowercase())
                    .collect();
                by_id
                    .entry(manifest.id.clone())
                    .and_modify(|info| {
                        info.description = manifest.description.clone();
                        info.extensions = extensions.clone();
                    })
                    .or_insert_with(|| w::PluginInfo {
                        id: manifest.id.clone(),
                        name: manifest.name.clone(),
                        version: manifest.version.clone(),
                        description: manifest.description.clone(),
                        enabled: true,
                        extensions,
                        path: loaded.package.root.to_string_lossy().into_owned(),
                        ..Default::default()
                    });
            }
            Ok(w::PluginList {
                plugins: by_id.into_values().collect(),
                ..Default::default()
            })
        })
        .await
    }

    async fn install_plugin(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::InstallPluginRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        let source_path = request.to_owned_message().source_path;
        let state_dir = self.runtime.state_dir().to_path_buf();
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let (entry, manifest) = crate::host::db::plugins::install_local_package(
                &store,
                &state_dir,
                std::path::Path::new(&source_path),
            )?;
            let extensions = manifest
                .entrypoints
                .iter()
                .flat_map(|ep| ep.provides.iter())
                .map(|ext| format!("{ext:?}").to_ascii_lowercase())
                .collect();
            Ok(w::PluginInfo {
                id: entry.plugin_id,
                name: entry.name,
                version: entry.version,
                description: manifest.description,
                enabled: entry.enabled,
                extensions,
                path: entry.path,
                installed_at: entry.installed_at,
                ..Default::default()
            })
        })
        .await
    }

    async fn set_plugin_enabled(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SetPluginEnabledRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        let req = request.to_owned_message();
        self.call(move |app| {
            let store = app.world().resource::<Store>();
            if !store.set_plugin_enabled(&req.plugin_id, req.enabled)? {
                return Err(HostError::NotFound(format!(
                    "plugin `{}` not found",
                    req.plugin_id
                )));
            }
            let entry = store
                .plugin_entries()?
                .into_iter()
                .find(|e| e.plugin_id == req.plugin_id)
                .ok_or_else(|| HostError::NotFound("plugin not found".into()))?;
            Ok(w::PluginInfo {
                id: entry.plugin_id,
                name: entry.name,
                version: entry.version,
                enabled: entry.enabled,
                path: entry.path,
                installed_at: entry.installed_at,
                ..Default::default()
            })
        })
        .await
    }

    async fn remove_plugin(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::RemovePluginRequest>,
    ) -> ServiceResult<w::Empty> {
        let plugin_id = request.to_owned_message().plugin_id;
        let state_dir = self.runtime.state_dir().to_path_buf();
        self.call(move |app| {
            let store = app.world().resource::<Store>();
            if !crate::host::db::plugins::remove_local_package(store, &state_dir, &plugin_id)? {
                return Err(HostError::NotFound(format!(
                    "plugin `{plugin_id}` not found"
                )));
            }
            Ok(w::Empty::default())
        })
        .await
    }
}

fn notify_settings(app: &mut App, settings: w::Settings) {
    app.world_mut().resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
            settings: Some(settings).into(),
            ..Default::default()
        })),
    );
}

fn is_builtin_voicevox(
    connection: &Connection,
    providers: &crate::host::providers::Providers,
) -> bool {
    connection.enabled
        && connection.kind == settings::ConnectionKind::Tts
        && connection
            .endpoint()
            .adapter
            .eq_ignore_ascii_case("voicevox")
        && !model::is_sakura_endpoint(&connection.endpoint().endpoint)
        && providers.get("voicevox").is_none()
}

fn rule_from(rule: w::PolicyRule) -> Result<permission::PolicyRule, HostError> {
    let effect = match rule.effect.as_known() {
        Some(w::PolicyEffect::Allow) => permission::PolicyEffect::Allow,
        Some(w::PolicyEffect::Deny) => permission::PolicyEffect::Deny,
        Some(w::PolicyEffect::Confirm) => permission::PolicyEffect::Confirm,
        Some(w::PolicyEffect::AskLlm) => permission::PolicyEffect::AskLlm,
        Some(w::PolicyEffect::Unspecified) | None => {
            return Err(HostError::Invalid(("a policy effect is required").into()));
        }
    };
    Ok(permission::PolicyRule {
        id: rule.id,
        tool_name: rule.tool_name,
        pattern: rule.pattern,
        effect,
        created_at: rule.created_at,
    })
}

async fn probe_voicevox_tts(
    connection: &crate::host::settings::Connection,
    voicevox: &crate::host::voice::voicevox::VoicevoxManager,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let base = connection.endpoint().endpoint.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "http://127.0.0.1:50021"
    } else {
        base
    };
    let url = format!("{base}/speakers");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return crate::host::settings::Check {
                healthy: false,
                checked_at,
                failure: Some(crate::host::error::Failure::new(
                    "connection_check",
                    error.to_string(),
                )),
                capabilities: Vec::new(),
                resources: Vec::new(),
            };
        }
    };

    let mut response = client.get(&url).send().await;
    if response.is_err()
        && let Err(start_error) = voicevox
            .ensure_running(base, &connection.endpoint().options)
            .await
    {
        return crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                start_error,
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        };
    } else if response.is_err() {
        response = client.get(&url).send().await;
    }

    match response {
        Ok(response) if response.status().is_success() => {
            let mut resources = Vec::new();
            if let Ok(speakers) = response.json::<Vec<serde_json::Value>>().await {
                for speaker in speakers {
                    let speaker_name = speaker
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .unwrap_or("");
                    if let Some(styles) = speaker.get("styles").and_then(|s| s.as_array()) {
                        for style in styles {
                            if let Some(id) = style.get("id").and_then(|v| v.as_u64()) {
                                let style_name = style
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map(str::trim)
                                    .unwrap_or("");
                                let entry = match (speaker_name.is_empty(), style_name.is_empty()) {
                                    (false, false) => {
                                        format!("{id}:{speaker_name}（{style_name}）")
                                    }
                                    (false, true) => format!("{id}:{speaker_name}"),
                                    _ => id.to_string(),
                                };
                                resources.push(entry);
                            }
                        }
                    }
                }
            }
            if resources.is_empty() {
                resources.extend([
                    "1:ずんだもん（あまあま）".to_owned(),
                    "3:ずんだもん（ノーマル）".to_owned(),
                ]);
            }
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: Vec::new(),
                resources,
            }
        }
        Ok(response) => crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                format!("HTTP {}", response.status().as_u16()),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
        Err(error) => crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                error.to_string(),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
    }
}

// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
async fn probe_openai_speech(
    connection: &crate::host::settings::Connection,
    credentials: &model::Credentials,
    providers: &crate::host::providers::Providers,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let fail = |message: String| crate::host::settings::Check {
        healthy: false,
        checked_at: checked_at.clone(),
        failure: Some(crate::host::error::Failure::new(
            "connection_check",
            message,
        )),
        capabilities: Vec::new(),
        resources: Vec::new(),
    };
    let base = settings::endpoint_of(connection, providers).unwrap_or_else(|| {
        connection
            .endpoint()
            .endpoint
            .trim()
            .trim_end_matches('/')
            .to_owned()
    });
    let Ok(parsed) = url::Url::parse(&base) else {
        return fail("接続先URLが正しくありません".into());
    };
    let sakura = model::is_sakura_connection(&connection.endpoint().adapter, &base);
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return fail(error.to_string()),
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => return fail(error.to_string()),
    };

    let models_url = if sakura && base.ends_with("/tts/v1") {
        let trimmed = base.trim_end_matches("/tts/v1");
        format!("{trimmed}/v1/models")
    } else {
        format!("{base}/models")
    };
    let mut req = client.get(&models_url);
    if let Some(key) = secret.as_deref() {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let mut listed = Vec::new();
            if let Ok(json) = resp.json::<serde_json::Value>().await
                && let Some(items) = json.get("data").and_then(|v| v.as_array())
            {
                for item in items {
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        listed.push(id.to_owned());
                    }
                }
            }
            let resources = if sakura && connection.kind == settings::ConnectionKind::Tts {
                let mut out: Vec<String> = model::SAKURA_TTS_VOICES
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect();
                for id in listed {
                    if model::is_sakura_tts_model_id(&id)
                        && !out.iter().any(|entry| {
                            entry
                                .split('/')
                                .next()
                                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&id))
                        })
                    {
                        out.push(format!("{id}/normal:VOICEVOX:{id}"));
                    }
                }
                out
            } else if sakura && connection.kind == settings::ConnectionKind::Stt {
                let mut out: Vec<String> = listed
                    .into_iter()
                    .filter(|id| {
                        let lower = id.to_ascii_lowercase();
                        lower.contains("whisper") || lower.contains("transcribe")
                    })
                    .collect();
                for id in model::SAKURA_STT_MODELS {
                    if !out.iter().any(|existing| existing == id) {
                        out.push((*id).to_owned());
                    }
                }
                out
            } else {
                listed
            };
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: Vec::new(),
                resources,
            }
        }
        Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => fail(format!(
            "認証エラー (HTTP {})。APIキー（アカウントトークン）を確認してください。",
            resp.status().as_u16()
        )),
        Ok(resp) => {
            let is_loopback = parsed.host().is_some_and(|h| match h {
                url::Host::Ipv4(ip) => ip.is_loopback(),
                url::Host::Ipv6(ip) => ip.is_loopback(),
                url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
            });
            if is_loopback {
                let origin = format!(
                    "{}://{}{}",
                    parsed.scheme(),
                    parsed.host_str().unwrap_or("127.0.0.1"),
                    parsed.port().map(|p| format!(":{p}")).unwrap_or_default()
                );
                for path in ["/health", "/"] {
                    let mut fallback = client.get(format!("{origin}{path}"));
                    if let Some(key) = secret.as_deref() {
                        fallback = fallback.bearer_auth(key);
                    }
                    if let Ok(r) = fallback.send().await
                        && r.status().is_success()
                    {
                        return crate::host::settings::Check {
                            healthy: true,
                            checked_at,
                            failure: None,
                            capabilities: Vec::new(),
                            resources: Vec::new(),
                        };
                    }
                }
            }
            fail(format!("HTTP {}", resp.status().as_u16()))
        }
        Err(error) => fail(error.to_string()),
    }
}

async fn probe_embedding(
    connection: &crate::host::settings::Connection,
    credentials: &model::Credentials,
    providers: &crate::host::providers::Providers,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let fail = |message: String, resources: Vec<String>| crate::host::settings::Check {
        healthy: false,
        checked_at: checked_at.clone(),
        failure: Some(crate::host::error::Failure::new(
            "connection_check",
            message,
        )),
        capabilities: Vec::new(),
        resources,
    };
    let Some(base) = settings::endpoint_of(connection, providers) else {
        return fail("接続先URLが正しくありません".into(), Vec::new());
    };
    if url::Url::parse(&base).is_err() {
        return fail("接続先URLが正しくありません".into(), Vec::new());
    }
    let sakura = model::is_sakura_connection(&connection.endpoint().adapter, &base);
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return fail(error.to_string(), Vec::new()),
    };

    let mut resources = Vec::new();
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        let mut req = client.get(format!("{base}/models"));
        if let Some(key) = secret.as_deref() {
            req = req.bearer_auth(key);
        }
        if let Ok(resp) = req.send().await {
            if matches!(resp.status().as_u16(), 401 | 403) {
                return fail(
                    format!(
                        "認証エラー (HTTP {})。APIキー（アカウントトークン）を確認してください。",
                        resp.status().as_u16()
                    ),
                    resources,
                );
            }
            if resp.status().is_success()
                && let Ok(json) = resp.json::<serde_json::Value>().await
                && let Some(items) = json.get("data").and_then(|v| v.as_array())
            {
                for item in items {
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        let lower = id.to_ascii_lowercase();
                        if lower.contains("embed") || lower.contains("e5") {
                            resources.push(id.to_owned());
                        }
                    }
                }
            }
        }
    }
    if sakura {
        for id in model::SAKURA_EMBEDDING_MODELS {
            if !resources.iter().any(|existing| existing == id) {
                resources.push((*id).to_owned());
            }
        }
    }

    match crate::host::mind::embedding::probe_dimensions(connection, credentials, providers).await {
        Ok(actual_dim) => {
            let configured_dim = match &connection.config {
                settings::ConnectionConfig::Embedding(cfg) => cfg.dimensions as usize,
                _ => 0,
            };
            if configured_dim > 0 && configured_dim != actual_dim {
                return crate::host::settings::Check {
                    healthy: false,
                    checked_at,
                    failure: Some(crate::host::error::Failure::new(
                        "connection_check",
                        format!(
                            "埋め込み次元数が一致しません（設定: {configured_dim} / モデルの出力: {actual_dim}）。次元数を {actual_dim} に設定してください。"
                        ),
                    )),
                    capabilities: vec![format!("dimensions:{actual_dim}")],
                    resources,
                };
            }
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: vec![format!("dimensions:{actual_dim}")],
                resources,
            }
        }
        Err(error) => fail(error, resources),
    }
}

fn model_item(id: String, spec: &ModelSpec) -> w::Model {
    let name = if spec.display_name.is_empty() {
        id.clone()
    } else {
        spec.display_name.clone()
    };
    w::Model {
        id,
        name,
        metadata: Some(w::ModelMetadata {
            source: "catalog".into(),
            scope: "model".into(),
            context_window: spec.context_window.map(u64::from),
            max_input_tokens: spec.input_limit().map(u64::from),
            max_output_tokens: spec.max_output_tokens.map(u64::from),
            input_modalities: Some(w::StringList {
                values: modality_names(&spec.input),
                ..Default::default()
            })
            .into(),
            output_modalities: Some(w::StringList {
                values: vec!["text".into()],
                ..Default::default()
            })
            .into(),
            supported_parameters: Some(w::StringList::default()).into(),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

fn modality_names(modalities: &rig_core::catalog::Modalities) -> Vec<String> {
    let present = [
        (modalities.text, "text"),
        (modalities.image, "image"),
        (modalities.audio, "audio"),
        (modalities.video, "video"),
        (modalities.pdf, "pdf"),
    ];
    present
        .into_iter()
        .filter(|(present, _)| *present)
        .map(|(_, name)| name.to_owned())
        .collect()
}
