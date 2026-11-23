//! Settings, connections, credentials, policy and approval services.
mod connections;
mod general;
mod models;
mod plugins;
mod policy;
mod probe;

use super::Services;
use crate::host::db::Store;
use crate::host::error::HostError;
use crate::host::model;
use crate::host::notify::Notifier;
use crate::host::permission;
use crate::host::settings;
use crate::host::settings::Connection;
use connectrpc::ConnectError;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;
use probe::probe_embedding;
use probe::probe_openai_speech;
use probe::probe_voicevox_tts;
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        general::get_settings(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        general::update_settings(self, arg1, request).await
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PendingApprovals> {
        connections::list_pending_approvals(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::DeleteConnectionRequest>,
    ) -> ServiceResult<w::Settings> {
        connections::delete_connection(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::SubmitApprovalRequest>,
    ) -> ServiceResult<w::SubmitApprovalResponse> {
        connections::submit_approval(self, arg1, request).await
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyRules> {
        policy::list_policy_rules(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyRulesRequest>,
    ) -> ServiceResult<w::PolicyRules> {
        policy::update_policy_rules(self, arg1, request).await
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyTexts> {
        policy::list_policy_texts(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyTextsRequest>,
    ) -> ServiceResult<w::PolicyTexts> {
        policy::update_policy_texts(self, arg1, request).await
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::LlmProviders> {
        models::list_providers(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        models::list_models(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        models::register_credentials(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        models::revoke_credentials(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        connections::check_connection(self, arg1, request).await
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
        arg1: RequestContext,
        arg2: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PluginList> {
        plugins::list_plugins(self, arg1, arg2).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::InstallPluginRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        plugins::install_plugin(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::SetPluginEnabledRequest>,
    ) -> ServiceResult<w::PluginInfo> {
        plugins::set_plugin_enabled(self, arg1, request).await
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
        arg1: RequestContext,
        request: ServiceRequest<'_, w::RemovePluginRequest>,
    ) -> ServiceResult<w::Empty> {
        plugins::remove_plugin(self, arg1, request).await
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
            if let Some(mut supervisor) = app
                .world_mut()
                .get_resource_mut::<crate::host::mcp::McpSupervisor>()
            {
                crate::host::mcp::request_reconnect(&mut supervisor, &reconnect_id);
            }
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

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn rule(effect: w::PolicyEffect) -> w::PolicyRule {
        w::PolicyRule {
            id: "rule-1".into(),
            tool_name: "files".into(),
            pattern: "**/*.md".into(),
            effect: effect.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        }
    }

    /// Every specified effect survives the conversion, so the host enforces
    /// what the client asked for rather than a default.
    #[test]
    fn each_effect_maps_to_its_own_policy_effect() {
        for (wire, expected) in [
            (w::PolicyEffect::Allow, permission::PolicyEffect::Allow),
            (w::PolicyEffect::Deny, permission::PolicyEffect::Deny),
            (w::PolicyEffect::Confirm, permission::PolicyEffect::Confirm),
            (w::PolicyEffect::AskLlm, permission::PolicyEffect::AskLlm),
        ] {
            let converted = rule_from(rule(wire)).expect("a specified effect converts");
            assert_eq!(converted.effect, expected, "{wire:?}");
        }
    }

    /// An unspecified effect is a client bug, not a request for the
    /// permissive default. Rejecting it keeps a malformed request from
    /// creating a rule that allows everything.
    #[test]
    fn an_unspecified_effect_is_rejected_rather_than_defaulted() {
        let error = rule_from(rule(w::PolicyEffect::Unspecified))
            .expect_err("Unspecified must not convert");
        assert!(
            matches!(error, HostError::Invalid(_)),
            "expected Invalid, got {error:?}"
        );
    }

    /// The identifying fields are carried across unchanged, since the rule is
    /// later matched by them.
    #[test]
    fn identifying_fields_survive_the_conversion() {
        let converted =
            rule_from(rule(w::PolicyEffect::Allow)).expect("a specified effect converts");
        assert_eq!(converted.id, "rule-1");
        assert_eq!(converted.tool_name, "files");
        assert_eq!(converted.pattern, "**/*.md");
        assert_eq!(converted.created_at, "2026-01-01T00:00:00Z");
    }
}
