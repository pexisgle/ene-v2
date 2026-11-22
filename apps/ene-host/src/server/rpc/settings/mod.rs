//! Settings, connections, credentials, policy and approval services.
use super::Services;
mod probe;
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
use probe::{probe_embedding, probe_openai_speech, probe_voicevox_tts};
use rig_core::catalog::ModelSpec;
use rig_ecs::prelude::App;

#[allow(refining_impl_trait)]
impl w::SettingsService for Services {
    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "get_settings",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        self.read(|app| {
            Ok(wire::settings(
                &app.world().resource::<Store>().settings_repo().settings()?,
            ))
        })
        .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "update_settings",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
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
                let settings = store
                    .settings_repo()
                    .update_settings(expected_revision, &inputs)?;
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
        if let Ok(current) = store.settings_repo().settings() {
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_pending_approvals",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn list_pending_approvals(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PendingApprovals> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "delete_connection",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            connection_id = %request.id,
        )
    )]
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
            let (settings, companions) = store.settings_repo().delete_connection(&id, revision, || {
                crate::host::model::delete_secret(&credentials, &id)
            })?;
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
                    .settings_repo()
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "submit_approval",
            companion_id = tracing::field::Empty,
            request_id = %request.request_id,
        )
    )]
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_policy_rules",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn list_policy_rules(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyRules> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyRules {
                items: store
                    .policy_repo()
                    .policy_rules()?
                    .iter()
                    .map(wire::policy_rule)
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "update_policy_rules",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
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
            let updated = store
                .policy_repo()
                .apply_policy_rules(&rules, &remove_ids)?;
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_policy_texts",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn list_policy_texts(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyTexts> {
        self.read(|app| {
            let store = app.world().resource::<Store>();
            Ok(w::PolicyTexts {
                items: store
                    .policy_repo()
                    .policy_texts()?
                    .iter()
                    .map(wire::policy_text)
                    .collect(),
                ..Default::default()
            })
        })
        .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "update_policy_texts",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
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
            let updated = store
                .policy_repo()
                .apply_policy_texts(&texts, &remove_ids)?;
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_providers",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_models",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            connection_id = %request.id,
        )
    )]
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
                .settings_repo()
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "register_credentials",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            connection_id = %request.id,
        )
    )]
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
            if store.settings_repo().connection(&id)?.is_none() {
                return Err(HostError::NotFound("connection not found".into()));
            }
            model::save_secret(&credentials, &id, &secret)?;
            store
                .settings_repo()
                .set_credential_state(&id, CredentialState::Registered)?;
            notify_settings(app, wire::settings(&store.settings_repo().settings()?));
            Ok(w::CredentialResult {
                credential_state: CredentialState::Registered.as_str().into(),
                ..Default::default()
            })
        })
        .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "revoke_credentials",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            connection_id = %request.id,
        )
    )]
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
                .settings_repo()
                .connection(&id)?
                .ok_or_else(|| HostError::NotFound("connection not found".into()))?;
            model::delete_secret(&credentials, &id)?;
            let state = model::credential_state(&connection, &credentials, &providers)?;
            store.settings_repo().set_credential_state(&id, state)?;
            notify_settings(app, wire::settings(&store.settings_repo().settings()?));
            Ok(w::Empty::default())
        })
        .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "check_connection",
            companion_id = tracing::field::Empty,
            request_id = %request.request_key,
            connection_id = %request.id,
        )
    )]
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
                    .settings_repo()
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
        let check = run_connection_probe(self, &store, &credentials, &connection).await?;
        let request_id = request.request_key.to_owned();
        let resource_id = request.resource_id.map(str::to_owned);
        let id = connection.id;
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
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
                notify_settings(app, wire::settings(&store.settings_repo().settings()?));
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_plugins",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn list_plugins(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PluginList> {
        let providers = self.runtime.providers();
        self.read(move |app| {
            let store = app.world().resource::<Store>();
            let entries = store.plugin_repo().plugin_entries()?;
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "install_plugin",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn install_plugin(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::InstallPluginRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        let source_path = request.to_owned_message().source_path;
        let state_dir = self.runtime.state_dir().to_path_buf();
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let (entry, manifest) = crate::host::providers::install_local_package(
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "set_plugin_enabled",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            plugin_id = %request.plugin_id,
        )
    )]
    async fn set_plugin_enabled(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SetPluginEnabledRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        let req = request.to_owned_message();
        self.call(move |app| {
            let store = app.world().resource::<Store>();
            if !store
                .plugin_repo()
                .set_plugin_enabled(&req.plugin_id, req.enabled)?
            {
                return Err(HostError::NotFound(format!(
                    "plugin `{}` not found",
                    req.plugin_id
                )));
            }
            let entry = store
                .plugin_repo()
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "remove_plugin",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            plugin_id = %request.plugin_id,
        )
    )]
    async fn remove_plugin(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::RemovePluginRequest>,
    ) -> ServiceResult<w::Empty> {
        let plugin_id = request.to_owned_message().plugin_id;
        let state_dir = self.runtime.state_dir().to_path_buf();
        self.call(move |app| {
            let store = app.world().resource::<Store>();
            if !crate::host::providers::remove_local_package(store, &state_dir, &plugin_id)? {
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

async fn run_connection_probe(
    services: &Services,
    store: &Store,
    credentials: &model::Credentials,
    connection: &Connection,
) -> std::result::Result<settings::Check, ConnectError> {
    let providers = services.runtime.providers();
    if connection.kind == settings::ConnectionKind::Mcp {
        return probe_mcp_connection(services, connection).await;
    }
    if connection.kind == settings::ConnectionKind::Tts
        && connection
            .endpoint()
            .adapter
            .eq_ignore_ascii_case("voicevox")
        && !model::is_sakura_endpoint(&connection.endpoint().endpoint)
        && providers.get("voicevox").is_none()
    {
        let voicevox = services.runtime.voicevox();
        return Ok(probe_voicevox_tts(connection, &voicevox).await);
    }
    if matches!(
        connection.kind,
        settings::ConnectionKind::Stt | settings::ConnectionKind::Tts
    ) {
        return Ok(probe_openai_speech(connection, credentials, &providers).await);
    }
    if connection.kind == settings::ConnectionKind::Embedding {
        return Ok(probe_embedding(connection, credentials, &providers).await);
    }
    if connection.kind == settings::ConnectionKind::Search {
        return Ok(crate::host::web_search::probe(connection, credentials, &providers).await);
    }
    Ok(model::probe(store, credentials, connection, &providers).await)
}

async fn probe_mcp_connection(
    services: &Services,
    connection: &Connection,
) -> std::result::Result<settings::Check, ConnectError> {
    if !connection.enabled {
        return Ok(settings::Check {
            healthy: false,
            checked_at: crate::host::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                "MCP 接続が無効化されています。",
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        });
    }
    let reconnect_id = connection.id.clone();
    services
        .call(move |app| {
            crate::host::mcp::request_reconnect(app.world_mut(), &reconnect_id);
            Ok(())
        })
        .await?;
    let uses_npx = matches!(
        &connection.config,
        settings::ConnectionConfig::Mcp(config) if crate::host::mcp::is_npx_command(&config.command)
    );
    // npm may have to download a package before the server can list tools.
    let timeout = std::time::Duration::from_secs(if uses_npx { 60 } else { 20 });
    let outcome = tokio::time::timeout(timeout, async {
        loop {
            let id = connection.id.clone();
            let revision = connection.revision;
            let state = services
                .runtime
                .read(move |app| {
                    let world = app.world();
                    if !world
                        .resource::<Store>()
                        .settings_repo()
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
        Ok(Ok(Ok(tools))) => Ok(settings::Check {
            healthy: true,
            checked_at: crate::host::util::now(),
            failure: None,
            capabilities: Vec::new(),
            resources: tools,
        }),
        Ok(Ok(Err(reason))) => Ok(settings::Check {
            healthy: false,
            checked_at: crate::host::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                format!("{default_message} ({reason})"),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        }),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Ok(settings::Check {
            healthy: false,
            checked_at: crate::host::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                default_message,
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        }),
    }
}
