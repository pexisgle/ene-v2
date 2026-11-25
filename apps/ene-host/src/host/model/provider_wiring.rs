//! Provider construction: which rig provider an adapter names, its registry
//! configuration, and the Sakura gateway dialect.
//!
//! Kept apart from model resolution so a new adapter touches one file: the
//! adapter name maps to a provider here, and every other module asks this one.
use super::agent_platform;
use ene_catalog::SAKURA_VISION_MODELS;
use rig_core::catalog::ModelSpec;
use rig_core::providers::registry::ProviderId;
use rig_core::providers::{
    openai::{
        OpenAIConfig,
        wire::{Dialect, DimensionsField, Quirks, Route},
    },
    registry::ProviderConfig,
};

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
        crate::host::settings::SAKURA_AI_ENDPOINT,
        crate::host::settings::SAKURA_AI_API_KEY_ENV,
    )
    .with_quirks(quirks)
}

pub(crate) fn sakura_model_spec(model_id: &str) -> Option<ModelSpec> {
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
    loaded: &crate::host::providers::Providers,
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
        crate::host::settings::provider(adapter, loaded).map_or_else(String::new, |p| p.endpoint);
    match provider_of(adapter).and_then(|provider| provider.config(secret.clone())) {
        Some(config) if base != default => config.with_base_url(base),
        Some(config) => config,
        None => ProviderConfig::OpenAi(OpenAIConfig::new(secret).with_base_url(base)),
    }
}
