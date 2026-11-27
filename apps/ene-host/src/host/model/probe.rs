//! Connection probing: does a configured LLM connection answer, and which
//! models does it serve?
//!
//! Split out of `mod.rs` because a probe is I/O with its own timeout and error
//! taxonomy, not model resolution. Callers reach it through `mod.rs`.
use super::*;

pub(crate) async fn probe_model(model: Result<DynModel<Completion>>, model_id: String) -> Check {
    use crate::host::error::Failure;
    use rig_core::completion::CompletionRequest;
    use rig_core::error::ErrorKind;

    let checked_at = ene_store::util::now();
    let failure = match model {
        Err(HostError::FailedPrecondition(_)) => {
            Some(("credentials", "認証情報を確認してください。"))
        }
        Err(_) => Some(("connection_setup", "モデルと接続設定を確認してください。")),
        Ok(model) => {
            let request =
                CompletionRequest::new(ene_domain::settings::MODEL_CHECK_PROMPT).max_tokens(64);
            match tokio::time::timeout(PROBE_TIMEOUT, model.call(request)).await {
                Ok(Ok(reply)) if !reply.text().trim().is_empty() => None,
                Ok(Ok(_)) => Some(("empty_reply", "モデルからテキスト応答がありませんでした。")),
                Err(_) => Some(("timeout", "モデルの応答が時間内にありませんでした。")),
                Ok(Err(error)) => Some(
                    match error
                        .provider_response_status()
                        .map(|status| status.as_u16())
                    {
                        Some(401) => ("credentials", "接続先が認証情報を受け付けませんでした。"),
                        Some(403) => (
                            "access",
                            "接続先がこのモデルへのアクセスを許可しませんでした。",
                        ),
                        Some(404) => ("model_unavailable", "接続先でこのモデルを利用できません。"),
                        Some(429) => (
                            "rate_limit",
                            "接続先の利用制限に達しました。時間をおいて再試行してください。",
                        ),
                        _ if error.report().kind == ErrorKind::Http => {
                            ("transport", "接続先から応答を受け取れませんでした。")
                        }
                        _ => (
                            "provider_response",
                            "接続先でモデルの応答を取得できませんでした。",
                        ),
                    },
                ),
            }
        }
    };
    Check {
        healthy: failure.is_none(),
        checked_at,
        failure: failure.map(|(kind, message)| Failure::new(kind, message)),
        checked_model: Some(model_id),
        ..Default::default()
    }
}

pub(crate) async fn probe(
    credentials: &Credentials,
    connection: &Connection,
    providers: &crate::host::providers::Providers,
) -> Check {
    let checked_at = ene_store::util::now();
    let failure = |message: String| Check {
        healthy: false,
        checked_at: checked_at.clone(),
        failure: Some(crate::host::error::Failure::new(
            "connection_check",
            message,
        )),
        capabilities: Vec::new(),
        resources: Vec::new(),
        checked_model: None,
    };
    let ConnectionConfig::Llm(config) = &connection.config else {
        // Only the LLM adapter has a probe in this alpha.
        return Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "unsupported",
                "connection checks are implemented for LLM connections only",
            )),
            ..Default::default()
        };
    };
    if config.endpoint.adapter == agent_platform::ADAPTER
        && !providers.claims(agent_platform::ADAPTER, ene_plugin::Extension::Llm)
    {
        let verified = |mut check: Check| {
            check
                .capabilities
                .push(agent_platform::VERIFIED_MODEL_LIST.into());
            check
        };
        let secret = match connection_secret(connection, Some(credentials), providers) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return verified(failure(
                    "Google Agent Platform の API キーが未登録です".into(),
                ));
            }
            Err(error) => return verified(failure(error.to_string())),
        };
        let result =
            match agent_platform::verify_models(&config.endpoint, &secret, &config.model_ids).await
            {
                Ok(result) => result,
                Err(error) => return verified(failure(error.to_string())),
            };
        let mut check = result.failure.map_or(
            Check {
                healthy: true,
                checked_at: checked_at.clone(),
                ..Default::default()
            },
            failure,
        );
        check.capabilities.push("model_list".into());
        check.resources = result.ids;
        return verified(check);
    }
    // A connection that leaves its endpoint empty calls the provider's own
    // default, which is exactly what the probe has to ask.
    let Some(base) = crate::host::settings::endpoint_of(connection, providers) else {
        return failure("the connection has no endpoint to probe".into());
    };
    let listed = match list_models(connection, &base, credentials, providers).await {
        Ok(models) => models,
        Err(error) => return failure(error.to_string()),
    };
    let listed = if is_sakura_connection(&connection.endpoint().adapter, &base) {
        let chat_only: Vec<String> = listed
            .iter()
            .filter(|id| !is_non_chat_sakura_model(id))
            .cloned()
            .collect();
        if !chat_only.is_empty() {
            chat_only
        } else if listed.is_empty() {
            SAKURA_CHAT_MODELS
                .iter()
                .map(|id| (*id).to_owned())
                .collect()
        } else {
            listed
        }
    } else {
        listed
    };
    let mut resources: Vec<String> = config.model_ids.clone();
    for model in listed {
        if resources.len() >= MAX_MODELS {
            break;
        }
        if !resources.iter().any(|known| known == &model) {
            resources.push(model);
        }
    }
    Check {
        healthy: true,
        checked_at,
        failure: None,
        capabilities: vec!["model_list".into(), "openai_http".into()],
        resources,
        checked_model: None,
    }
}

#[derive(Debug, thiserror::Error)]
enum ModelProbeError {
    #[error("the plugin did not answer in {0}s")]
    PluginTimeout(u64),
    #[error("OpenRouterのAPIキーが未登録です")]
    MissingOpenRouterKey,
    #[error("OpenRouterのキーを確認できませんでした（{0}）")]
    OpenRouterKeyRejected(reqwest::StatusCode),
    #[error("the endpoint did not answer in {0}s")]
    EndpointTimeout(u64),
    #[error("the endpoint is unreachable: {0}")]
    EndpointUnreachable(String),
    #[error("the plugin returned no Models event")]
    MissingModelsEvent,
    #[error(transparent)]
    Net(#[from] net::NetError),
    #[error("{0}")]
    Failed(String),
}

/// Asks the provider's own model-listing endpoint, through rig's provider
/// clients, so the probe reads the wire the connection is configured for
/// instead of assuming an OpenAI-shaped `/models` reply.
async fn list_models(
    connection: &Connection,
    base: &str,
    credentials: &Credentials,
    providers: &crate::host::providers::Providers,
) -> std::result::Result<Vec<String>, ModelProbeError> {
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin lists its own models, through the same `ListModels` operation
    // the agent loop would use, so the probe reads what the plugin serves.
    if providers.claims(adapter, ene_plugin::Extension::Llm) {
        return tokio::time::timeout(PROBE_TIMEOUT, async {
            let running = providers
                .start(adapter, ene_plugin::Extension::Llm)
                .await
                .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
            let secret = connection_secret(connection, Some(credentials), providers)
                .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
            let context = crate::host::providers::ProviderInvocationContext::from_connection(
                &running,
                connection,
                secret,
                providers,
                Some(crate::host::net::MODEL_TIMEOUT),
            )
            .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
            plugin_models(running, context).await
        })
        .await
        .map_err(|_| ModelProbeError::PluginTimeout(PROBE_TIMEOUT.as_secs()))?;
    }
    let secret = connection_secret(connection, Some(credentials), providers)
        .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
    // OpenRouter's model list is public even when the supplied key is invalid.
    if adapter == "openrouter" && base.trim_end_matches('/') == "https://openrouter.ai/api/v1" {
        let key = secret
            .as_deref()
            .ok_or(ModelProbeError::MissingOpenRouterKey)?;
        let response = net::client()?
            .get("https://openrouter.ai/api/v1/key")
            .bearer_auth(key)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
        if !response.status().is_success() {
            return Err(ModelProbeError::OpenRouterKeyRejected(response.status()));
        }
    }
    let config = provider_config(adapter, base, secret, providers);
    let http = rig_reqwest::ReqwestClient::from(net::client()?);
    let list = tokio::time::timeout(PROBE_TIMEOUT, async move {
        match &config {
            ProviderConfig::OpenAi(config) => config.clone().connect(http).list_models().await,
            ProviderConfig::Anthropic(config) => config.clone().connect(http).list_models().await,
            ProviderConfig::Gemini(config) => config.clone().connect(http).list_models().await,
        }
    })
    .await
    .map_err(|_| ModelProbeError::EndpointTimeout(PROBE_TIMEOUT.as_secs()))?
    .map_err(|error| ModelProbeError::EndpointUnreachable(error.to_string()))?;
    Ok(list.data.into_iter().map(|model| model.id).collect())
}

/// Asks a provider plugin for the models it serves.
///
/// The plugin answers `ListModels` with its `Models` event, which the runtime
/// validates before the host sees it; this reads that event out of the stream
/// the call produced.
async fn plugin_models(
    running: Arc<crate::host::providers::Running>,
    context: crate::host::providers::ProviderInvocationContext,
) -> std::result::Result<Vec<String>, ModelProbeError> {
    let (sender, mut events) = tokio::sync::mpsc::channel(16);
    let services = context.services();
    let (result, models) = tokio::join!(
        running.invoke_streaming(
            ene_plugin::Operation::ListModels {
                endpoint: context.endpoint,
                options: context.options,
            },
            services,
            sender,
        ),
        async move {
            let mut models = None;
            while let Some(event) = events.recv().await {
                if let ene_plugin::Event::Models { models: listed } = event {
                    models = Some(listed);
                }
            }
            models
        }
    );
    result.map_err(|error| ModelProbeError::Failed(error.to_string()))?;
    models
        .map(|models| models.into_iter().map(|model| model.id).collect())
        .ok_or(ModelProbeError::MissingModelsEvent)
}
