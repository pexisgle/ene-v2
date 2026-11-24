//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
pub(crate) mod agent_platform;
pub(crate) mod catalog;
mod plugin;
#[cfg(test)]
pub(crate) mod plugin_tests;

use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::net;
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind, CredentialState};
pub(crate) use plugin::completion as plugin_completion;
use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
use rig_core::driver::{DynModel, Model};
use rig_core::operation::Completion;
use rig_core::providers::{
    openai::{
        OpenAIConfig,
        wire::{Dialect, DimensionsField, Quirks, Route},
    },
    registry::{ConnectOptions, ProviderConfig, ProviderId},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const KEYRING_SERVICE: &str = "ene-host";
const MAX_MODELS: usize = 4_096;

/// How long the model-list probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Credentials {
    session_secrets: Arc<Mutex<HashMap<String, String>>>,
    generation: Arc<AtomicU64>,
    session_only: bool,
    /// Test-only: makes the credential store unreadable, so a test can check
    /// that the failure is reported instead of becoming "no credential".
    #[cfg(test)]
    unreadable: Arc<std::sync::atomic::AtomicBool>,
}

impl Credentials {
    pub(crate) fn new(session_only: bool) -> Self {
        Self {
            session_only,
            ..Self::default()
        }
    }

    fn session_secrets(&self) -> MutexGuard<'_, HashMap<String, String>> {
        super::lock(&self.session_secrets)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn remember(&self, connection: &str, secret: String) {
        self.session_secrets().insert(connection.to_owned(), secret);
        self.invalidate();
    }

    pub(crate) fn forget(&self, connection: &str) {
        self.session_secrets().remove(connection);
        self.invalidate();
    }

    /// The registered secret, `None` when none is registered. An unreadable
    /// credential store is an error, not "no credential": a request must not
    /// go out unauthenticated because the store could not be read.
    pub(crate) fn secret(&self, connection: &str) -> Result<Option<String>> {
        if self.session_only {
            return Ok(self.session_secrets().get(connection).cloned());
        }
        #[cfg(test)]
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(HostError::Unavailable(
                "credential store: unreadable".into(),
            ));
        }
        read_keyring(connection)
    }

    #[cfg(test)]
    pub(crate) fn make_unreadable(&self) {
        self.unreadable
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn session_only(&self) -> bool {
        self.session_only
    }
}

fn keyring_entry(connection: &str) -> std::result::Result<keyring::Entry, HostError> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("connection:{connection}"))
        .map_err(|error| HostError::Unavailable(format!("credential store: {error}")))
}

fn read_keyring(connection: &str) -> std::result::Result<Option<String>, HostError> {
    match keyring_entry(connection)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(HostError::Unavailable(format!("credential store: {error}"))),
    }
}

/// Stores a secret in the keyring (or only in memory for a session-only host).
pub(crate) fn save_secret(credentials: &Credentials, connection: &str, secret: &str) -> Result<()> {
    if secret.is_empty() || secret.chars().count() > 8_192 {
        return Err(HostError::Invalid(
            ("credential must be 1-8192 characters").into(),
        ));
    }
    if !credentials.session_only() {
        keyring_entry(connection)?
            .set_password(secret)
            .map_err(|error| HostError::Unavailable(format!("credential store: {error}")))?;
    }
    if credentials.session_only() {
        credentials.remember(connection, secret.to_owned());
    } else {
        credentials.invalidate();
    }
    Ok(())
}

pub(crate) fn delete_secret(credentials: &Credentials, connection: &str) -> Result<()> {
    if !credentials.session_only() {
        match keyring_entry(connection)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(HostError::Unavailable(format!("credential store: {error}"))),
        }
    }
    credentials.forget(connection);
    Ok(())
}

/// The state the contract reports for a connection's secret.
pub(crate) fn credential_state(
    connection: &Connection,
    credentials: &Credentials,
    loaded: &super::providers::Providers,
) -> Result<CredentialState> {
    if credentials.secret(&connection.id)?.is_some() {
        return Ok(CredentialState::Registered);
    }
    if super::settings::environment_variable(connection, loaded)
        .is_some_and(|name| std::env::var_os(name).is_some())
    {
        return Ok(CredentialState::Environment);
    }
    Ok(CredentialState::Absent)
}

pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
    loaded: &super::providers::Providers,
) -> Result<Option<String>> {
    if let Some(credentials) = credentials
        && let Some(secret) = credentials.secret(&connection.id)?
    {
        return Ok(Some(secret));
    }
    Ok(super::settings::environment_variable(connection, loaded)
        .and_then(|name| std::env::var(name).ok())
        .filter(|secret| !secret.is_empty()))
}

use ene_catalog::SAKURA_VISION_MODELS;
pub(crate) use ene_catalog::{
    SAKURA_CHAT_MODELS, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES,
    is_non_chat_sakura_model, is_sakura_connection, is_sakura_endpoint, is_sakura_tts_model_id,
};

pub(crate) fn sakura_dialect() -> Dialect {
    let mut quirks = Quirks::openai();
    // Sakura's Responses API lacks multi-turn conversation state; Chat Completions is full-featured.
    quirks.completion_route = Route::Chat;
    // Sakura's OpenAPI spec returns embeddings without a mandatory `usage` block and documents
    // fixed native dimensions per model without an OpenAI `dimensions` parameter.
    quirks.embedding.requires_usage = false;
    quirks.embedding.dimensions = DimensionsField::Ignored;
    Dialect::gateway(
        "sakura",
        super::settings::SAKURA_AI_ENDPOINT,
        super::settings::SAKURA_AI_API_KEY_ENV,
    )
    .with_quirks(quirks)
}

fn sakura_model_spec(model_id: &str) -> Option<ModelSpec> {
    let provider = ProviderId::declare("sakura")
        .ok()
        .or_else(|| ProviderId::catalog("openai"))?;
    let mut spec = ModelSpec::new(provider, model_id).with_tools(true);
    spec.input.image = SAKURA_VISION_MODELS.contains(&model_id);
    Some(spec)
}

/// The rig provider a host adapter names.
///
pub(crate) fn provider_of(adapter: &str) -> Option<ProviderId> {
    ProviderId::catalog(match adapter {
        "gemini" => "gcp.gemini",
        agent_platform::ADAPTER => "vertexai",
        adapter => adapter,
    })
}

/// Registry configuration for listing and embedding, including the host's
/// OpenAI-compatible fallback for adapters without a registered wire.
pub(crate) fn provider_config(
    adapter: &str,
    base: &str,
    secret: Option<String>,
    loaded: &super::providers::Providers,
) -> ProviderConfig {
    let secret = secret.unwrap_or_default();
    if is_sakura_connection(adapter, base) {
        return ProviderConfig::OpenAi(
            OpenAIConfig::with_key(&sakura_dialect(), secret)
                .with_base_url(base)
                .with_route(Route::Chat),
        );
    }
    let default =
        super::settings::provider(adapter, loaded).map_or_else(String::new, |p| p.endpoint);
    match provider_of(adapter).and_then(|provider| provider.config(secret.clone())) {
        Some(config) if base != default => config.with_base_url(base),
        Some(config) => config,
        None => ProviderConfig::OpenAi(OpenAIConfig::new(secret).with_base_url(base)),
    }
}

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

/// Probes the connection's model endpoint. A failure is reported, never
/// treated as healthy.
pub(crate) async fn probe(
    _store: &Store,
    credentials: &Credentials,
    connection: &Connection,
    providers: &super::providers::Providers,
) -> Check {
    let checked_at = super::util::now();
    let failure = |message: String| Check {
        healthy: false,
        checked_at: checked_at.clone(),
        failure: Some(super::error::Failure::new("connection_check", message)),
        capabilities: Vec::new(),
        resources: Vec::new(),
    };
    let ConnectionConfig::Llm(config) = &connection.config else {
        // Only the LLM adapter has a probe in this alpha.
        return Check {
            healthy: false,
            checked_at,
            failure: Some(super::error::Failure::new(
                "unsupported",
                "connection checks are implemented for LLM connections only",
            )),
            ..Default::default()
        };
    };
    if config.endpoint.adapter == agent_platform::ADAPTER
        && providers.get(agent_platform::ADAPTER).is_none()
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
        check
            .capabilities
            .extend(["model_list".into(), "streaming_text".into()]);
        check.resources = result.ids;
        return verified(check);
    }
    // A connection that leaves its endpoint empty calls the provider's own
    // default, which is exactly what the probe has to ask.
    let Some(base) = super::settings::endpoint_of(connection, providers) else {
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
        capabilities: vec![
            "streaming_text".into(),
            "model_list".into(),
            "openai_http".into(),
        ],
        resources,
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
    providers: &super::providers::Providers,
) -> std::result::Result<Vec<String>, ModelProbeError> {
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin lists its own models, through the same `ListModels` operation
    // the agent loop would use, so the probe reads what the plugin serves.
    if providers.get(adapter).is_some() {
        return tokio::time::timeout(PROBE_TIMEOUT, async {
            let running = providers
                .start(adapter, ene_plugin::Extension::Llm)
                .await
                .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
            let secret = connection_secret(connection, Some(credentials), providers)
                .map_err(|error| ModelProbeError::Failed(error.to_string()))?;
            let context = super::providers::ProviderInvocationContext::from_connection(
                &running,
                connection,
                secret,
                providers,
                Some(super::net::MODEL_TIMEOUT),
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
    running: Arc<super::providers::Running>,
    context: super::providers::ProviderInvocationContext,
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

#[cfg(test)]
mod tests;
