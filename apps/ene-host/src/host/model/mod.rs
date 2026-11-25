//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
pub(crate) mod agent_platform;
pub(crate) mod catalog;
mod credentials;
mod plugin;
mod probe;
mod provider_wiring;

pub(crate) use credentials::{
    Credentials, connection_secret, credential_state, delete_secret, save_secret,
};
pub(crate) use probe::probe;
pub(crate) use provider_wiring::{
    SAKURA_CHAT_MODELS, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES,
    is_non_chat_sakura_model, is_sakura_connection, is_sakura_endpoint, is_sakura_tts_model_id,
    provider_config, provider_of, sakura_dialect, sakura_model_spec,
};
#[cfg(test)]
pub(crate) mod plugin_tests;

use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::net;
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind};
pub(crate) use plugin::completion as plugin_completion;
use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
use rig_core::driver::{DynModel, Model};
use rig_core::operation::Completion;
use rig_core::providers::{
    openai::{OpenAIConfig, wire::Route},
    registry::{ConnectOptions, ProviderConfig, ProviderId},
};
use std::{sync::Arc, time::Duration};

const MAX_MODELS: usize = 4_096;

/// How long the model-list probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

type ConnectModel = dyn Fn(
        &Store,
        &Credentials,
        &Catalog,
        &Companion,
        Role,
        &super::providers::Providers,
    ) -> Result<DynModel<Completion>>
    + Send
    + Sync;

#[derive(Clone, bevy_ecs::prelude::Resource)]
pub(crate) struct ModelFactory(pub(crate) Arc<ConnectModel>);

impl Default for ModelFactory {
    fn default() -> Self {
        Self(Arc::new(connect))
    }
}

/// Builds the rig model for one role of a companion.
pub(crate) fn connect(
    store: &Store,
    credentials: &Credentials,
    catalog: &Catalog,
    companion: &Companion,
    role: Role,
    providers: &super::providers::Providers,
) -> Result<DynModel<Completion>> {
    let assignment = companion
        .model(role)
        .ok_or_else(|| HostError::Unavailable(("no model is assigned to this role").into()))?;
    connect_assignment(store, credentials, catalog, assignment, role, providers)
}

/// Builds a rig completion model directly from one `Assignment`.
pub(crate) fn connect_assignment(
    store: &Store,
    credentials: &Credentials,
    catalog: &Catalog,
    assignment: &super::mind::Assignment,
    _role: Role,
    providers: &super::providers::Providers,
) -> Result<DynModel<Completion>> {
    let connection = store
        .settings_repo()
        .connection(&assignment.connection_id)?
        .ok_or_else(|| HostError::Unavailable(("the assigned connection does not exist").into()))?;
    if !connection.enabled {
        return Err(HostError::Unavailable(
            ("the assigned connection is disabled").into(),
        ));
    }
    if connection.kind != ConnectionKind::Llm {
        return Err(HostError::Unavailable(
            ("the assigned connection is not an LLM connection").into(),
        ));
    }
    let endpoint = connection.endpoint();
    // A plugin that serves this connection's adapter answers instead of the
    // catalog, so a provider rig does not ship needs no host change.
    if let Some(running) = providers.running(&endpoint.adapter, ene_plugin::Extension::Llm) {
        let secret = connection_secret(&connection, Some(credentials), providers)?;
        let context = super::providers::ProviderInvocationContext::from_connection(
            &running,
            &connection,
            secret,
            providers,
            Some(super::net::MODEL_TIMEOUT),
        )?;
        let role = running.provider().ok_or_else(|| {
            HostError::Unavailable(("the extension serves no provider to call").into())
        })?;
        return Ok(plugin_completion(
            Arc::clone(&running),
            role,
            context,
            assignment.model_id.clone(),
            catalog
                .resolve(&assignment.model_id)
                .is_ok_and(|entry| entry.spec.input.image),
        ));
    }
    let base = super::settings::endpoint_of(&connection, providers)
        .ok_or_else(|| HostError::Unavailable(("the connection has no endpoint to call").into()))?;
    if is_sakura_connection(&endpoint.adapter, &base) {
        let secret = connection_secret(&connection, Some(credentials), providers)?;
        let client =
            net::model_client().map_err(|error| HostError::Unavailable(error.to_string()))?;
        let http = rig_reqwest::ReqwestClient::from(client);
        let config = OpenAIConfig::with_key(&sakura_dialect(), secret.unwrap_or_default())
            .with_base_url(base)
            .with_route(Route::Chat);
        let spec = Arc::new(
            sakura_model_spec(&assignment.model_id)
                .ok_or_else(|| HostError::Internal("sakura provider is unavailable".into()))?,
        );
        let chat = config
            .chat(assignment.model_id.clone())
            .with_facts(ModelFacts::new(spec).with_catalog(catalog));
        return Ok(Model::new(chat, http).erase());
    }
    let provider = provider_of(&endpoint.adapter).ok_or_else(|| {
        HostError::Unavailable(format!("unsupported adapter `{}`", endpoint.adapter))
    })?;
    // The adapter's label names the connection; the catalog files the model
    // under the provider's rig name, which differs for `gemini`.
    let catalog_reference = format!("{}/{}", provider.vendor(), assignment.model_id);
    let spec = match catalog.resolve(&catalog_reference) {
        Ok(resolved) => resolved.shared(),
        Err(_) if endpoint.adapter == agent_platform::ADAPTER => catalog
            .resolve(&format!("gcp.gemini/{}", assignment.model_id))
            .map_or_else(
                |_| Arc::new(ModelSpec::new(provider, &assignment.model_id).with_tools(true)),
                |resolved| resolved.shared(),
            ),
        Err(_) => {
            let mut spec = ModelSpec::new(provider, &assignment.model_id).with_tools(true);
            if endpoint.adapter == "openrouter" {
                spec.input.image = catalog
                    .resolve(&assignment.model_id)
                    .is_ok_and(|entry| entry.spec.input.image);
            }
            Arc::new(spec)
        }
    };
    let secret = connection_secret(&connection, Some(credentials), providers)?;
    if endpoint.adapter == agent_platform::ADAPTER {
        return agent_platform::model(endpoint, secret, spec, catalog);
    }
    let default = super::settings::provider(&endpoint.adapter, providers)
        .map_or_else(String::new, |p| p.endpoint);
    // Rig builds the model requests, so their deadline sits on the model
    // client; every other host request shares the unbounded one.
    let client = net::model_client().map_err(|error| HostError::Unavailable(error.to_string()))?;
    let mut options = ConnectOptions::new().http(rig_reqwest::ReqwestClient::from(client));
    if let Some(secret) = secret {
        options = options.api_key(secret);
    }
    // rig is handed the base URL only when it is not the provider's own, so a
    // connection that leaves the field empty keeps the provider default.
    if base != default {
        options = options.base_url(base.clone());
    }
    let model = catalog
        .connect_with(spec.as_ref(), options)
        .map_err(|error| HostError::Unavailable(format!("model connection: {error}")))?;
    Ok(model)
}

/// Picks either the requested `Assignment` or the first enabled LLM connection + model
/// for ephemeral preview without mutating companion/session state.
pub(crate) fn preview_assignment(
    store: &Store,
    catalog: &Catalog,
    requested: &super::mind::Assignment,
) -> Result<super::mind::Assignment> {
    if requested.is_set() {
        return Ok(requested.clone());
    }
    let settings = store.settings_repo().settings()?;
    let connection = settings
        .connections
        .into_iter()
        .find(|conn| conn.enabled && conn.kind == ConnectionKind::Llm)
        .ok_or_else(|| {
            HostError::FailedPrecondition(
                ("an enabled LLM connection is required for preview").into(),
            )
        })?;
    let model_id = models(&connection, catalog)
        .into_iter()
        .map(|(id, _)| id)
        .next()
        .ok_or_else(|| {
            HostError::FailedPrecondition(("no model is available on the LLM connection").into())
        })?;
    Ok(super::mind::Assignment {
        connection_id: connection.id,
        model_id,
    })
}

/// Runs a single stateless, tool-less completion call for companion preview.
pub(crate) async fn preview_reply(
    model: DynModel<Completion>,
    system_prompt: String,
    user_message: String,
) -> Result<String> {
    use rig_core::completion::CompletionRequest;

    let prompt = if user_message.trim().is_empty() {
        "はじめまして！自己紹介と最初の挨拶を1〜2文でお願いします。".to_owned()
    } else {
        user_message
    };
    let request = CompletionRequest::new(prompt)
        .preamble(system_prompt)
        .max_tokens(512);
    let response = tokio::time::timeout(Duration::from_secs(20), model.call(request))
        .await
        .map_err(|_| HostError::Unavailable(("the preview model request timed out").into()))?
        .map_err(|error| HostError::Unavailable(format!("preview completion failed: {error}")))?;
    Ok(response.text().trim().to_owned())
}

/// Adapter-default model candidates when neither `model_ids` nor `last_check`
/// has populated a concrete list, ordered with reliable tool-capable defaults first.
fn default_models_for(adapter: &str) -> &'static [&'static str] {
    match adapter {
        "openrouter" | "openrouter-wasm" => &[
            "openai/gpt-4o-mini",
            "openai/gpt-4o",
            "anthropic/claude-3.5-haiku",
            "google/gemini-2.5-flash",
        ],
        "openai" => &["gpt-4o-mini", "gpt-4o"],
        "anthropic" => &["claude-3-5-haiku-latest", "claude-sonnet-4-20250514"],
        "gemini" => &["gemini-2.5-flash", "gemini-2.5-pro"],
        agent_platform::ADAPTER => &[agent_platform::DEFAULT_MODEL],
        "sakura" => SAKURA_CHAT_MODELS,
        _ => &[],
    }
}

/// Lists the models a connection serves, putting explicit `model_ids` first,
/// then probed `last_check.resources` (promoting known tool-capable defaults
/// first among probed results), and falling back to adapter defaults only when
/// both are empty.
pub(crate) fn models(connection: &Connection, catalog: &Catalog) -> Vec<(String, ModelSpec)> {
    let ConnectionConfig::Llm(config) = &connection.config else {
        return Vec::new();
    };
    let adapter = connection.endpoint().adapter.as_str();
    let preferred = default_models_for(adapter);
    let verified = adapter == agent_platform::ADAPTER
        && connection.last_check.as_ref().is_some_and(|check| {
            check
                .capabilities
                .iter()
                .any(|cap| cap == agent_platform::VERIFIED_MODEL_LIST)
        });
    let resources = connection
        .last_check
        .as_ref()
        .map_or(&[][..], |check| check.resources.as_slice());

    let mut ordered = Vec::new();
    fn push_unique_id(ordered: &mut Vec<String>, id: &str) {
        if ordered.len() < MAX_MODELS && !ordered.iter().any(|existing| existing == id) {
            ordered.push(id.to_owned());
        }
    }

    for id in &config.model_ids {
        let trimmed = id.trim();
        if !trimmed.is_empty() && (!verified || resources.iter().any(|id| id == trimmed)) {
            push_unique_id(&mut ordered, trimmed);
        }
    }
    if !resources.is_empty() {
        for &pref in preferred {
            if let Some(found) = resources.iter().find(|id| id.as_str() == pref) {
                push_unique_id(&mut ordered, found);
            }
        }
        for id in resources {
            push_unique_id(&mut ordered, id);
        }
    } else if ordered.is_empty() && !verified {
        for &id in preferred {
            push_unique_id(&mut ordered, id);
        }
    }

    ordered
        .into_iter()
        .take(MAX_MODELS)
        .filter_map(|model| {
            let spec = if is_sakura_connection(adapter, &connection.endpoint().endpoint) {
                sakura_model_spec(&model)?
            } else {
                let provider = provider_of(adapter).or_else(|| ProviderId::catalog("openai"))?;
                let reference = format!("{}/{}", provider.vendor(), model);
                let resolved = catalog.resolve(&reference).ok().or_else(|| {
                    (adapter == agent_platform::ADAPTER)
                        .then(|| catalog.resolve(&format!("gcp.gemini/{model}")).ok())
                        .flatten()
                });
                resolved.map_or_else(
                    || ModelSpec::new(provider, &model).with_tools(true),
                    |resolved| resolved.spec.clone(),
                )
            };
            Some((model, spec))
        })
        .collect()
}

#[cfg(test)]
mod tests;
