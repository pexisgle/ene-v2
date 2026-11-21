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
    if connection
        .environment_variable(loaded)
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
    Ok(connection
        .environment_variable(loaded)
        .and_then(|name| std::env::var(name).ok())
        .filter(|secret| !secret.is_empty()))
}

pub(crate) const SAKURA_CHAT_MODELS: &[&str] = &[
    "gpt-oss-120b",
    "llm-jp-3.1-8x13b-instruct4",
    "preview/Qwen3.6-35B-A3B",
    "preview/gemma-4-31B-it",
    "preview/Kimi-K2.6",
    "preview/Kimi-K2.7-Code",
    "preview/Qwen3-VL-30B-A3B-Instruct",
    "preview/GLM-5.3-Flash",
    "preview/DeepSeek-V4.1-Flash-1017",
    "preview/Phi-4-mini-instruct-cpu",
    "preview/Qwen3-0.6B-cpu",
];

// https://manual.sakura.ad.jp/cloud/ai-engine/chat-models.html
const SAKURA_VISION_MODELS: &[&str] = &[
    "preview/gemma-4-31B-it",
    "preview/Qwen3-VL-30B-A3B-Instruct",
    "preview/Qwen3.6-35B-A3B",
    "preview/Kimi-K2.7-Code",
    "preview/GLM-5.3-Flash",
    "preview/DeepSeek-V4.1-Flash-1017",
];

pub(crate) const SAKURA_STT_MODELS: &[&str] = &["whisper-large-v3-turbo"];

pub(crate) const SAKURA_EMBEDDING_MODELS: &[&str] =
    &["multilingual-e5-large", "preview/Qwen3-Embedding-4B-FP16"];

pub(crate) const SAKURA_TTS_VOICES: &[&str] = &[
    "zundamon/normal:VOICEVOX:ずんだもん（ノーマル）",
    "zundamon/amaama:VOICEVOX:ずんだもん（あまあま）",
    "zundamon/tsuntsun:VOICEVOX:ずんだもん（ツンツン）",
    "zundamon/sexy:VOICEVOX:ずんだもん（セクシー）",
    "zundamon/sasayaki:VOICEVOX:ずんだもん（ささやき）",
    "shikokumetan/normal:VOICEVOX:四国めたん（ノーマル）",
    "shikokumetan/amaama:VOICEVOX:四国めたん（あまあま）",
    "shikokumetan/tsuntsun:VOICEVOX:四国めたん（ツンツン）",
    "shikokumetan/sexy:VOICEVOX:四国めたん（セクシー）",
    "kasukabetsumugi/normal:VOICEVOX:春日部つむぎ（ノーマル）",
    "meimeihimari/normal:VOICEVOX:冥鳴ひまり（ノーマル）",
    "tohokuzunko/normal:VOICEVOX:東北ずん子（ノーマル）",
    "tohokukiritan/normal:VOICEVOX:東北きりたん（ノーマル）",
    "tohokuitako/normal:VOICEVOX:東北イタコ（ノーマル）",
    "ankomon/normal:VOICEVOX:あんこもん（ノーマル）",
];

pub(crate) fn is_sakura_endpoint(url: &str) -> bool {
    url::Url::parse(url.trim())
        .ok()
        .and_then(|parsed| {
            parsed
                .host_str()
                .map(|host| host.eq_ignore_ascii_case("api.ai.sakura.ad.jp"))
        })
        .unwrap_or(false)
}

pub(crate) fn is_sakura_connection(adapter: &str, endpoint: &str) -> bool {
    adapter.eq_ignore_ascii_case("sakura") || is_sakura_endpoint(endpoint)
}

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

pub(crate) fn is_sakura_tts_model_id(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "zundamon"
            | "shikokumetan"
            | "kasukabetsumugi"
            | "meimeihimari"
            | "tohokuzunko"
            | "tohokukiritan"
            | "tohokuitako"
            | "ankomon"
            | "zunko"
            | "kiritan"
            | "itako"
            | "himari"
    )
}

pub(crate) fn is_non_chat_sakura_model(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    lower.contains("whisper")
        || lower.contains("transcribe")
        || lower.contains("embedding")
        || lower.contains("multilingual-e5")
        || is_sakura_tts_model_id(&lower)
}

fn sakura_model_spec(model_id: &str) -> ModelSpec {
    let provider = ProviderId::declare("sakura").expect("valid provider name");
    let mut spec = ModelSpec::new(provider, model_id).with_tools(true);
    spec.input.image = SAKURA_VISION_MODELS.contains(&model_id);
    spec
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
        let context = super::providers::ProviderInvocationContext::from_connection(
            &running,
            &connection,
            Some(credentials),
            providers,
            Some(super::net::MODEL_TIMEOUT),
        )?;
        return Ok(plugin_completion(
            running,
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
        let client = net::model_client().map_err(HostError::Unavailable)?;
        let http = rig_reqwest::ReqwestClient::from(client);
        let config = OpenAIConfig::with_key(&sakura_dialect(), secret.unwrap_or_default())
            .with_base_url(base)
            .with_route(Route::Chat);
        let spec = Arc::new(sakura_model_spec(&assignment.model_id));
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
            .map(|resolved| resolved.shared())
            .unwrap_or_else(|_| {
                Arc::new(ModelSpec::new(provider, &assignment.model_id).with_tools(true))
            }),
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
    let client = net::model_client().map_err(HostError::Unavailable)?;
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
    let settings = store.settings()?;
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
        .map(|check| check.resources.as_slice())
        .unwrap_or(&[]);

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
        .map(|model| {
            let spec = if is_sakura_connection(adapter, &connection.endpoint().endpoint) {
                sakura_model_spec(&model)
            } else {
                let provider = provider_of(adapter).unwrap_or_else(|| {
                    ProviderId::catalog("openai").expect("openai is registered")
                });
                let reference = format!("{}/{}", provider.vendor(), model);
                let resolved = catalog.resolve(&reference).ok().or_else(|| {
                    (adapter == agent_platform::ADAPTER)
                        .then(|| catalog.resolve(&format!("gcp.gemini/{model}")).ok())
                        .flatten()
                });
                resolved
                    .map(|resolved| resolved.spec.clone())
                    .unwrap_or_else(|| ModelSpec::new(provider, &model).with_tools(true))
            };
            (model, spec)
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
    let checked_at = super::db::now();
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
                Err(error) => return verified(failure(error)),
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
        Err(error) => return failure(error),
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

/// Asks the provider's own model-listing endpoint, through rig's provider
/// clients, so the probe reads the wire the connection is configured for
/// instead of assuming an OpenAI-shaped `/models` reply.
async fn list_models(
    connection: &Connection,
    base: &str,
    credentials: &Credentials,
    providers: &super::providers::Providers,
) -> std::result::Result<Vec<String>, String> {
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin lists its own models, through the same `ListModels` operation
    // the agent loop would use, so the probe reads what the plugin serves.
    if providers.get(adapter).is_some() {
        return tokio::time::timeout(PROBE_TIMEOUT, async {
            let running = providers
                .start(adapter, ene_plugin::Extension::Llm)
                .await
                .map_err(|error| error.to_string())?;
            let context = super::providers::ProviderInvocationContext::from_connection(
                &running,
                connection,
                Some(credentials),
                providers,
                Some(super::net::MODEL_TIMEOUT),
            )
            .map_err(|error| error.to_string())?;
            plugin_models(running, context).await
        })
        .await
        .map_err(|_| format!("the plugin did not answer in {}s", PROBE_TIMEOUT.as_secs()))?;
    }
    let secret = connection_secret(connection, Some(credentials), providers)
        .map_err(|error| error.to_string())?;
    // OpenRouter's model list is public even when the supplied key is invalid.
    if adapter == "openrouter" && base.trim_end_matches('/') == "https://openrouter.ai/api/v1" {
        let key = secret.as_deref().ok_or("OpenRouterのAPIキーが未登録です")?;
        let response = net::client()?
            .get("https://openrouter.ai/api/v1/key")
            .bearer_auth(key)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "OpenRouterのキーを確認できませんでした（{}）",
                response.status()
            ));
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
    .map_err(|_| {
        format!(
            "the endpoint did not answer in {}s",
            PROBE_TIMEOUT.as_secs()
        )
    })?
    .map_err(|error| format!("the endpoint is unreachable: {error}"))?;
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
) -> std::result::Result<Vec<String>, String> {
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
    result.map_err(|error| error.to_string())?;
    models
        .map(|models| models.into_iter().map(|model| model.id).collect())
        .ok_or_else(|| "the plugin returned no Models event".to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{Endpoint, LlmConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn llm_connection(adapter: &str, endpoint: &str, models: &[&str]) -> Connection {
        Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: adapter.into(),
                    endpoint: endpoint.into(),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_ids: models.iter().map(|model| (*model).to_owned()).collect(),
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    fn connection_for(adapter: &str, endpoint: &str) -> Connection {
        llm_connection(adapter, endpoint, &["gpt-5-mini"])
    }

    /// A store that cannot be read must not look like a connection with no
    /// credential: the request path has to report the failure instead of
    /// sending an unauthenticated request or falling back to the environment.
    #[test]
    fn an_unreadable_credential_store_is_not_an_absent_credential() {
        let credentials = Credentials::new(false);
        credentials.make_unreadable();
        let connection = connection_for("openai", "http://127.0.0.1:1234");
        let providers = super::super::providers::Providers::none();
        assert!(matches!(
            credentials.secret(&connection.id),
            Err(HostError::Unavailable(_))
        ));
        assert!(connection_secret(&connection, Some(&credentials), &providers).is_err());
        assert!(credential_state(&connection, &credentials, &providers).is_err());
    }

    #[test]
    fn registry_configs_keep_native_defaults_and_custom_endpoints() {
        let providers = super::super::providers::Providers::none();
        for adapter in ["openai", "openrouter", "anthropic", "gemini"] {
            let base = super::super::settings::provider(adapter, &providers)
                .unwrap()
                .endpoint;
            let preset = provider_of(adapter).unwrap().config("key").unwrap();
            let config = provider_config(adapter, &base, Some("key".into()), &providers);
            assert_eq!(config, preset, "{adapter}");
            let custom = provider_config(
                adapter,
                "http://127.0.0.1:1234",
                Some("key".into()),
                &providers,
            );
            assert_eq!(
                custom,
                preset.with_base_url("http://127.0.0.1:1234"),
                "{adapter}"
            );
        }
        assert_eq!(
            provider_config("custom", "http://127.0.0.1:1234", None, &providers),
            ProviderConfig::OpenAi(OpenAIConfig::new("").with_base_url("http://127.0.0.1:1234"))
        );
    }

    #[tokio::test]
    async fn completion_keeps_catalog_overrides_and_unknown_tool_support() {
        use crate::host::{mind::Assignment, tests::Harness};
        use rig_core::catalog::Pricing;

        let harness = Harness::new([]);
        let store = harness.store();
        let mut companion = store.companion(&harness.companion).unwrap().unwrap();
        let credentials = Credentials::new(true);
        let providers = super::super::providers::Providers::none();
        let mut overrides = Catalog::default();
        let original = Catalog::builtin()
            .resolve("openai/gpt-4o-mini")
            .unwrap()
            .spec
            .clone();
        let override_pricing = Pricing::new(1.25, 5.0);
        let spec = original.clone().with_pricing(override_pricing);
        overrides.insert(spec.clone());
        let catalog = Catalog::builtin().with_overrides(&overrides);
        for adapter in ["openai", "gemini"] {
            let mut connection = connection_for(adapter, "http://127.0.0.1:1234");
            connection.id = adapter.into();
            credentials.remember(adapter, "explicit-key".into());
            store
                .update_settings(store.settings_revision().unwrap(), &[(connection, 0)])
                .unwrap();
            for id in ["gpt-4o-mini", "ene-unlisted-model"] {
                companion.assignments.companion = Assignment {
                    connection_id: adapter.into(),
                    model_id: id.into(),
                };
                let model = connect(
                    &store,
                    &credentials,
                    &catalog,
                    &companion,
                    Role::Companion,
                    &providers,
                )
                .unwrap();
                let connected = model.spec().unwrap();
                assert_eq!(connected.id, id);
                assert_eq!(connected.provider, provider_of(adapter).unwrap());
                if adapter == "openai" && id == "gpt-4o-mini" {
                    let pricing = connected.pricing.as_ref().unwrap();
                    assert_eq!(
                        (pricing.input, pricing.output),
                        (override_pricing.input, override_pricing.output)
                    );
                    assert_eq!(
                        pricing.cache_read,
                        original.pricing.as_ref().unwrap().cache_read
                    );
                    assert_eq!(connected.context_window, original.context_window);
                } else {
                    assert!(connected.tools);
                }
            }
        }
    }

    #[test]
    fn sakura_perception_exposes_vision_only_for_supported_models() {
        use crate::host::{mind::Assignment, tests::Harness};

        let harness = Harness::new([]);
        let store = harness.store();
        let mut companion = store.companion(&harness.companion).unwrap().unwrap();
        let connection = connection_for("sakura", "http://127.0.0.1:1234");
        let connection_id = connection.id.clone();
        store
            .update_settings(store.settings_revision().unwrap(), &[(connection, 0)])
            .unwrap();
        for (id, supports_image) in [
            ("preview/DeepSeek-V4.1-Flash-1017", true),
            ("gpt-oss-120b", false),
        ] {
            companion.assignments.perception = Some(Assignment {
                connection_id: connection_id.clone(),
                model_id: id.into(),
            });
            let model = connect(
                &store,
                &Credentials::new(true),
                Catalog::builtin(),
                &companion,
                Role::Perception,
                &super::super::providers::Providers::none(),
            )
            .unwrap();
            let spec = model
                .spec()
                .expect("sakura completion model must expose catalog facts");
            assert_eq!(spec.id, id);
            assert_eq!(spec.provider.vendor(), "sakura");
            assert!(spec.tools);
            assert_eq!(spec.input.image, supports_image);
        }
    }

    /// Every built-in LLM provider binds through `agent::ensure_model` for
    /// both companion and task roles, including catalog-unlisted models and
    /// provider-specific models returned by model listing.
    #[test]
    fn all_builtin_providers_bind_through_ensure_model_for_companion_and_task() {
        use crate::host::{agent, mind::Assignment, notify::Notifier};
        use rig_ecs::model::{Connection as ModelConnection, Models};

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::new(true);
        let cases: &[(&str, &str, &str)] = &[
            ("openai", "https://api.openai.com/v1", "ene-unlisted-openai"),
            (
                "openrouter",
                "https://openrouter.ai/api/v1",
                "vendor/ene-unlisted-openrouter",
            ),
            (
                "anthropic",
                "https://api.anthropic.com",
                "claude-ene-unlisted",
            ),
            (
                "gemini",
                "https://generativelanguage.googleapis.com",
                "gemini-ene-unlisted",
            ),
            (
                agent_platform::ADAPTER,
                agent_platform::ENDPOINT,
                "gemini-4-neon",
            ),
            (
                agent_platform::ADAPTER,
                agent_platform::ENDPOINT,
                agent_platform::DEFAULT_MODEL,
            ),
            (
                "sakura",
                super::super::settings::SAKURA_AI_ENDPOINT,
                "gpt-oss-120b",
            ),
            (
                "sakura",
                super::super::settings::SAKURA_AI_ENDPOINT,
                "preview/DeepSeek-V4.1-Flash-1017",
            ),
        ];

        for (index, (adapter, endpoint, model_id)) in cases.iter().enumerate() {
            let conn_id = format!("conn-{index}");
            let mut connection = llm_connection(adapter, endpoint, &[*model_id]);
            connection.id = conn_id.clone();
            credentials.remember(&conn_id, "test-secret".into());
            store
                .update_settings(store.settings_revision().unwrap(), &[(connection, 0)])
                .unwrap();

            let assignment = Assignment {
                connection_id: conn_id,
                model_id: (*model_id).to_owned(),
            };
            let mut world = bevy_ecs::prelude::World::new();
            world.insert_resource(store.clone());
            world.insert_resource(credentials.clone());
            world.insert_resource(super::super::providers::Providers::none());
            world.insert_resource(ModelFactory::default());
            world.insert_resource(Models::default());
            world.insert_resource(Notifier::new());

            let created = crate::host::mind::create(
                &mut world,
                ene_rpc::CreateCompanionRequest {
                    name: format!("Ene-{index}"),
                    mind: "helpful".into(),
                    assignments: Some(crate::host::wire::assignments(
                        &crate::host::mind::Assignments {
                            companion: assignment.clone(),
                            task: assignment,
                            ..Default::default()
                        },
                    ))
                    .into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let companion = store.companion(&created.id).unwrap().unwrap();
            let companion_entity =
                agent::agent_of(&mut world, &companion.id).expect("companion agent");
            agent::ensure_model(&mut world, companion_entity).unwrap_or_else(|error| {
                panic!("companion ensure_model failed for {adapter}/{model_id}: {error}")
            });
            let companion_spec = &world.get::<ModelConnection>(companion_entity).unwrap().spec;
            assert_eq!(companion_spec.id, *model_id, "{adapter}/{model_id}");
            assert!(companion_spec.tools, "{adapter}/{model_id}");

            let work_entity = agent::spawn_work_agent(
                &mut world,
                &companion,
                &format!("work-{index}"),
                &format!("run-{index}"),
                "do work",
            );
            agent::ensure_model(&mut world, work_entity).unwrap_or_else(|error| {
                panic!("task ensure_model failed for {adapter}/{model_id}: {error}")
            });
            let work_spec = &world.get::<ModelConnection>(work_entity).unwrap().spec;
            assert_eq!(work_spec.id, *model_id, "{adapter}/{model_id}");
            assert!(work_spec.tools, "{adapter}/{model_id}");
        }
    }

    #[tokio::test]
    async fn sakura_companion_binds_and_answers_over_chat_completions() {
        use crate::host::{agent, mind::Assignment, notify::Notifier};
        use rig_core::{
            completion::CompletionRequest,
            effect::{Answered, EffectKind},
            serve::serve_inline,
        };
        use rig_ecs::model::{Connection as ModelConnection, Models};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..read]);
            assert!(
                request.starts_with("POST /v1/chat/completions "),
                "unexpected request: {request}"
            );
            let body = serde_json::json!({
                "id": "chatcmpl-sakura-1",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "gpt-oss-120b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "こんにちは！" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::new(true);
        let mut connection = llm_connection("sakura", &format!("http://{addr}/v1"), &[]);
        connection.id = "sakura-conn".into();
        credentials.remember("sakura-conn", "sakura-key".into());
        store
            .update_settings(store.settings_revision().unwrap(), &[(connection, 0)])
            .unwrap();

        let assignment = Assignment {
            connection_id: "sakura-conn".into(),
            model_id: "gpt-oss-120b".into(),
        };
        let mut world = bevy_ecs::prelude::World::new();
        world.insert_resource(store);
        world.insert_resource(credentials);
        world.insert_resource(super::super::providers::Providers::none());
        world.insert_resource(ModelFactory::default());
        world.insert_resource(Models::default());
        world.insert_resource(Notifier::new());

        let created = crate::host::mind::create(
            &mut world,
            ene_rpc::CreateCompanionRequest {
                name: "Ene".into(),
                mind: "helpful".into(),
                assignments: Some(crate::host::wire::assignments(
                    &crate::host::mind::Assignments {
                        companion: assignment.clone(),
                        task: assignment,
                        ..Default::default()
                    },
                ))
                .into(),
                ..Default::default()
            },
        )
        .unwrap();
        let entity = agent::agent_of(&mut world, &created.id).expect("companion agent");
        let bound = world
            .get::<ModelConnection>(entity)
            .expect("sakura companion model must bind without catalog-facts error");
        assert_eq!(bound.spec.id, "gpt-oss-120b");
        assert_eq!(bound.spec.provider.vendor(), "sakura");
        assert!(bound.spec.tools);

        let handler = bound.handler.0.clone();
        let answer = serve_inline(
            &handler,
            EffectKind::Completion {
                request: CompletionRequest::new("hello").into(),
                stream: false,
            },
        )
        .await
        .unwrap();
        let Answered::Completion(answer) = answer else {
            panic!("expected completion answer");
        };
        assert_eq!(answer.text(), "こんにちは！");
        server.await.unwrap();
    }

    /// Serves the model listing the request path asks for: Gemini's native
    /// reply under `/v1beta/models`, the OpenAI-shaped one elsewhere.
    async fn models_endpoint() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 4_096];
                let Ok(read) = socket.read(&mut request).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&request[..read]);
                sent.send(request.to_string()).unwrap();
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body = if path.starts_with("/v1beta/models") {
                    r#"{"models":[{"name":"models/gemini-probe"}]}"#
                } else {
                    r#"{"data":[{"id":"probe-model","display_name":"Probe model"}]}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (address, received)
    }

    #[test]
    fn session_secrets_are_shared_by_clones_but_isolated_between_instances() {
        let first = Credentials::new(true);
        let first_clone = first.clone();
        let second = Credentials::new(true);
        let second_clone = second.clone();
        let connection = "same-connection";

        assert_eq!(first.secret(connection).unwrap(), None);
        assert_eq!(second.secret(connection).unwrap(), None);

        save_secret(&first, connection, "first-secret").unwrap();
        assert_eq!(
            first.secret(connection).unwrap().as_deref(),
            Some("first-secret")
        );
        assert_eq!(
            first_clone.secret(connection).unwrap().as_deref(),
            Some("first-secret")
        );
        assert_eq!(second.secret(connection).unwrap(), None);
        assert_eq!(second_clone.secret(connection).unwrap(), None);

        save_secret(&second_clone, connection, "second-secret").unwrap();
        assert_eq!(
            second.secret(connection).unwrap().as_deref(),
            Some("second-secret")
        );
        assert_eq!(
            first.secret(connection).unwrap().as_deref(),
            Some("first-secret")
        );

        delete_secret(&first_clone, connection).unwrap();
        assert_eq!(first.secret(connection).unwrap(), None);
        assert_eq!(first_clone.secret(connection).unwrap(), None);
        assert_eq!(
            second.secret(connection).unwrap().as_deref(),
            Some("second-secret")
        );
        assert_eq!(
            second_clone.secret(connection).unwrap().as_deref(),
            Some("second-secret")
        );

        delete_secret(&second_clone, connection).unwrap();
        assert_eq!(second.secret(connection).unwrap(), None);
        assert_eq!(second_clone.secret(connection).unwrap(), None);
    }

    /// The probe lists models through the configured endpoint whichever
    /// adapter names it: `openai` and `openrouter` have registry presets,
    /// `gemini` maps to rig's `gcp.gemini` provider and reads its native
    /// listing, and a custom adapter falls back to the OpenAI-shaped wire.
    #[tokio::test]
    async fn the_probe_lists_models_for_any_adapter() {
        let (address, mut requests) = models_endpoint().await;
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::new(true);
        credentials.remember("c1", "explicit-key".into());
        for (adapter, model, path, authentication) in [
            (
                "openai",
                "probe-model",
                "/models",
                "authorization: Bearer explicit-key",
            ),
            (
                "openrouter",
                "probe-model",
                "/models",
                "authorization: Bearer explicit-key",
            ),
            (
                "anthropic",
                "probe-model",
                "/v1/models",
                "x-api-key: explicit-key",
            ),
            (
                "gemini",
                "gemini-probe",
                "/v1beta/models",
                "key=explicit-key",
            ),
            (
                "custom",
                "probe-model",
                "/models",
                "authorization: Bearer explicit-key",
            ),
        ] {
            let endpoint = format!("http://{address}");
            let connection = connection_for(adapter, &endpoint);
            let check = probe(
                &store,
                &credentials,
                &connection,
                &super::super::providers::Providers::none(),
            )
            .await;
            assert!(check.healthy, "{adapter}: {:?}", check.failure);
            let request = requests.recv().await.unwrap();
            let uri = request.split_whitespace().nth(1).unwrap();
            assert_eq!(uri.split('?').next().unwrap(), path, "{adapter}");
            assert!(request.contains(authentication), "{adapter}: {request}");
            assert!(
                check.resources.iter().any(|known| known == model),
                "{adapter}: {:?}",
                check.resources
            );
        }
    }

    #[test]
    fn models_merges_configured_and_probed_resources_without_duplicates() {
        let mut conn = llm_connection("openai", "https://api.openai.com/v1", &["gpt-5.4-mini"]);
        conn.last_check = Some(Check {
            healthy: true,
            checked_at: "2026-11-19T00:00:00Z".into(),
            failure: None,
            capabilities: Vec::new(),
            resources: vec!["gpt-4o".into(), "gpt-5.4-mini".into(), "gpt-5".into()],
        });
        let ids: Vec<String> = models(&conn, Catalog::builtin())
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec!["gpt-5.4-mini", "gpt-4o", "gpt-5"]);
    }

    #[test]
    fn sakura_dialect_uses_chat_route_and_permissive_embedding_quirks() {
        let providers = super::super::providers::Providers::none();
        assert!(is_sakura_connection("sakura", "https://custom.invalid/v1"));
        assert!(is_sakura_connection(
            "openai",
            "https://api.ai.sakura.ad.jp/v1/"
        ));
        assert!(!is_sakura_connection("openai", "https://api.openai.com/v1"));

        let ProviderConfig::OpenAi(cfg) = provider_config(
            "sakura",
            super::super::settings::SAKURA_AI_ENDPOINT,
            Some("tok".into()),
            &providers,
        ) else {
            panic!("expected OpenAi provider config for sakura");
        };
        assert_eq!(cfg.completion_route(), Route::Chat);
        assert!(!cfg.dialect.quirks.embedding.requires_usage);
        assert_eq!(
            cfg.dialect.quirks.embedding.dimensions,
            DimensionsField::Ignored
        );
    }

    #[tokio::test]
    async fn sakura_probe_filters_non_chat_models_from_llm_list() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let body = serde_json::json!({
                "object": "list",
                "data": [
                    {"id": "gpt-oss-120b"},
                    {"id": "whisper-large-v3-turbo"},
                    {"id": "multilingual-e5-large"},
                    {"id": "zundamon"},
                    {"id": "preview/Qwen3.6-35B-A3B"},
                ]
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        });

        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::new(true);
        credentials.remember("c1", "uuid:secret".into());
        let conn = llm_connection("sakura", &format!("http://{addr}/v1"), &[]);
        let check = probe(
            &store,
            &credentials,
            &conn,
            &super::super::providers::Providers::none(),
        )
        .await;
        assert!(check.healthy, "{:?}", check.failure);
        assert_eq!(
            check.resources,
            vec!["gpt-oss-120b", "preview/Qwen3.6-35B-A3B"]
        );
    }
}
