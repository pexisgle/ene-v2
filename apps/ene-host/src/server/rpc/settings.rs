//! Settings, connections, credentials, policy and approval services.
use super::Services;
use crate::host::{
    Runtime,
    db::Store,
    error::HostError,
    model,
    notify::Notifier,
    permission, settings,
    settings::{
        AcpConnection, Connection, ConnectionConfig, ConnectionKind, CredentialState,
        EmbeddingConnection, Endpoint, LlmConnection, McpConnection, SttConnection, TtsConnection,
    },
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
            inputs.push((connection_from(input)?, expected));
        }
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            let settings = store.update_settings(expected_revision, &inputs, &[])?;
            let view = wire::settings(&settings);
            notify_settings(app, view.clone());
            Ok(view)
        })
        .await
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

    async fn submit_approval(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubmitApprovalRequest>,
    ) -> ServiceResult<w::SubmitApprovalResponse> {
        let request = request.to_owned_message();
        let decision = match request.decision.to_i32() {
            1 => permission::ApprovalDecision::Approve,
            2 => permission::ApprovalDecision::AlwaysApprove,
            3 => permission::ApprovalDecision::Deny,
            _ => permission::ApprovalDecision::Unspecified,
        };
        let request_id = request.request_id;
        self.call(move |app| {
            let approval = permission::submit(app.world_mut(), &request_id, decision)?;
            Ok(w::SubmitApprovalResponse {
                acknowledged: true,
                status: wire::approval_status(approval.status).into(),
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

    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::LlmProviders> {
        Response::ok(w::LlmProviders {
            items: settings::providers()
                .into_iter()
                .map(|provider| wire::provider(&provider))
                .collect(),
            ..Default::default()
        })
    }

    async fn list_models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        let connection = connection(&self.runtime, request.id).await?;
        Response::ok(w::Models {
            items: model::models(&connection)
                .into_iter()
                .map(|(id, spec)| model_item(id, &spec))
                .collect(),
            ..Default::default()
        })
    }

    async fn register_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let request = request.to_owned_message();
        let id = request.id;
        let secret = request
            .secret
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| ConnectError::invalid_argument("secret is required"))?;
        connection(&self.runtime, &id).await?;
        let credentials = self.runtime.credentials();
        {
            let credentials = credentials.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || model::save_secret(&credentials, &id, &secret))
                .await
                .map_err(|_| ConnectError::internal("credential store task failed"))?
                .map_err(ConnectError::from)?;
        }
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            store.set_credential_state(&id, CredentialState::Registered)?;
            notify_settings(app, wire::settings(&store.settings()?));
            Ok(w::CredentialResult {
                credential_state: wire::credential_state(CredentialState::Registered).into(),
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
        let connection = connection(&self.runtime, &id).await?;
        let credentials = self.runtime.credentials();
        {
            let credentials = credentials.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || model::delete_secret(&credentials, &id))
                .await
                .map_err(|_| ConnectError::internal("credential store task failed"))?
                .map_err(ConnectError::from)?;
        }
        let state = model::credential_state(&connection, &credentials);
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            store.set_credential_state(&id, state)?;
            notify_settings(app, wire::settings(&store.settings()?));
            Ok(w::Empty::default())
        })
        .await
    }

    async fn check_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        let request = request.to_owned_message();
        let id = request.id.clone();
        let (connection, store) = self
            .runtime
            .read(move |app| {
                let store = app.world().resource::<Store>().clone();
                let connection = store
                    .connection(&id)?
                    .ok_or_else(|| HostError::not_found("connection not found"))?;
                Ok((connection, store))
            })
            .await
            .map_err(ConnectError::from)?;
        let credentials = self.runtime.credentials();
        // The probe performs a network request, so it must not run on the ECS
        // thread; the connection was captured above.
        let check = model::probe(&store, &credentials, &connection).await;
        let request_id = request.request_key;
        let resource_id = request.resource_id;
        let id = connection.id;
        self.call(move |app| {
            let store = app.world().resource::<Store>().clone();
            store.set_check(&id, &check)?;
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

async fn connection(runtime: &Runtime, id: &str) -> Result<Connection, ConnectError> {
    let id = id.to_owned();
    runtime
        .read(move |app| {
            let store = app.world().resource::<Store>();
            store
                .connection(&id)?
                .ok_or_else(|| HostError::not_found("connection not found"))
        })
        .await
        .map_err(ConnectError::from)
}

fn connection_from(input: w::ConnectionInput) -> Result<Connection, HostError> {
    let kind = kind_from(input.kind.to_i32())
        .ok_or_else(|| HostError::invalid("connection kind is required"))?;
    let config = input
        .config
        .into_option()
        .and_then(config_from)
        .ok_or_else(|| HostError::invalid("connection config is required"))?;
    Ok(Connection {
        id: input.id,
        name: input.name,
        enabled: input.enabled,
        kind,
        config,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    })
}

fn kind_from(kind: i32) -> Option<ConnectionKind> {
    Some(match kind {
        1 => ConnectionKind::Llm,
        2 => ConnectionKind::Tts,
        3 => ConnectionKind::Stt,
        4 => ConnectionKind::Embedding,
        5 => ConnectionKind::Mcp,
        6 => ConnectionKind::Acp,
        _ => return None,
    })
}

fn config_from(value: w::ConnectionConfig) -> Option<ConnectionConfig> {
    Some(match value.config? {
        w::connection_config::Config::Llm(config) => ConnectionConfig::Llm(LlmConnection {
            endpoint: endpoint_from(config.endpoint.as_option()),
            model_ids: config.model_ids,
        }),
        w::connection_config::Config::Tts(config) => ConnectionConfig::Tts(TtsConnection {
            endpoint: endpoint_from(config.endpoint.as_option()),
            voice_ids: config.voice_ids,
        }),
        w::connection_config::Config::Stt(config) => ConnectionConfig::Stt(SttConnection {
            endpoint: endpoint_from(config.endpoint.as_option()),
            language_hints: config.language_hints,
        }),
        w::connection_config::Config::Embedding(config) => {
            ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: endpoint_from(config.endpoint.as_option()),
                model_id: config.model_id,
                dimensions: config.dimensions,
            })
        }
        w::connection_config::Config::Mcp(config) => ConnectionConfig::Mcp(McpConnection {
            endpoint: endpoint_from(config.endpoint.as_option()),
            allowed_tools: config.allowed_tools,
        }),
        w::connection_config::Config::Acp(config) => ConnectionConfig::Acp(AcpConnection {
            endpoint: endpoint_from(config.endpoint.as_option()),
            command: config.command,
            args: config.args,
        }),
    })
}

fn endpoint_from(value: Option<&w::ConnectionEndpoint>) -> Endpoint {
    value.map_or_else(Endpoint::default, |value| Endpoint {
        adapter: value.adapter.clone(),
        endpoint: value.endpoint.clone(),
        options_schema: value.options_schema.clone(),
        options: value.options.clone(),
    })
}

fn rule_from(rule: w::PolicyRule) -> Result<permission::PolicyRule, HostError> {
    let effect = match rule.effect.to_i32() {
        1 => permission::PolicyEffect::Allow,
        2 => permission::PolicyEffect::Deny,
        3 => permission::PolicyEffect::Confirm,
        _ => return Err(HostError::invalid("a policy effect is required")),
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
