//! Connections and settings: domain definitions from `ene_domain::settings` plus
//! host-side built-in and plugin provider catalog helpers.

use super::model::agent_platform;
use crate::host::providers::Providers;
use ene_catalog::SearchAdapter;
pub(crate) use ene_catalog::{SAKURA_AI_API_KEY_ENV, SAKURA_AI_ENDPOINT};
#[cfg(test)]
pub(crate) use ene_domain::settings::{
    AcpConnection, EmbeddingConnection, LlmConnection, McpConnection, SearchConnection,
    SttConnection, TtsConnection,
};
pub(crate) use ene_domain::settings::{
    Check, Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint,
};
use ene_plugin::{Extension, provider::AuthProfile};
use ene_rpc as w;

/// Environment credentials apply to the provider's advertised endpoint.
pub(crate) fn environment_variables(connection: &Connection, loaded: &Providers) -> Vec<String> {
    let endpoint = connection.endpoint();
    if connection.kind == ConnectionKind::Mcp {
        return Vec::new();
    }
    if connection.kind == ConnectionKind::Search {
        return SearchAdapter::parse(&endpoint.adapter)
            .map(SearchAdapter::provider)
            .filter(|provider| {
                provider
                    .default_endpoint
                    .is_some_and(|default| matches_provider_endpoint(&endpoint.endpoint, default))
            })
            .and_then(|provider| provider.key_environment)
            .map(str::to_owned)
            .into_iter()
            .collect();
    }
    let plugin = plugin_entry(&endpoint.adapter, loaded);
    let is_native = plugin.is_none();
    let provider = plugin.or_else(|| builtin_provider(&endpoint.adapter));
    let Some(provider) = provider
        .filter(|provider| matches_provider_endpoint(&endpoint.endpoint, &provider.endpoint))
    else {
        return Vec::new();
    };
    if is_native && let Some(native) = super::model::provider_of(&endpoint.adapter) {
        let names = native.api_key_envs();
        if !names.is_empty() {
            return names.into_iter().map(str::to_owned).collect();
        }
    }
    provider.environment_variable.into_iter().collect()
}

fn matches_provider_endpoint(configured: &str, default: &str) -> bool {
    let configured = if configured.trim().is_empty() {
        default
    } else {
        configured
    };
    url::Url::parse(configured.trim().trim_end_matches('/'))
        .ok()
        .zip(url::Url::parse(default.trim().trim_end_matches('/')).ok())
        .is_some_and(|(configured, default)| configured == default)
}

/// One built-in LLM provider adapter in the contract's own shape. TTS/STT/MCP/ACP
/// use connection configs. The catalogue is derived, never stored, so there is
/// no second representation to keep in step with the wire.
fn catalog_entry(id: &str, name: &str, settings: &[(&str, &str)]) -> Option<w::LlmProvider> {
    let provider = super::model::provider_of(id)?;
    let config = provider.config("")?;
    Some(w::LlmProvider {
        id: id.into(),
        name: name.into(),
        adapter: id.into(),
        endpoint: config.base_url().into(),
        settings: settings
            .iter()
            .map(|(key, label)| w::SettingField {
                key: (*key).into(),
                label: (*label).into(),
                kind: "string".into(),
                ..Default::default()
            })
            .collect(),
        requires_credentials: provider.requires_credential(),
        environment_variable: provider.api_key_envs().first().map(|name| (*name).into()),
        ..Default::default()
    })
}

/// The provider catalogue the host can speak to through rig-core. It is a
/// fixed alpha list; extensions add adapters, not host code paths.
fn builtin_providers() -> Vec<w::LlmProvider> {
    let mut entries: Vec<w::LlmProvider> = [
        catalog_entry(
            "openai",
            "OpenAI",
            &[("base_url", "Base URL"), ("organization", "Organization")],
        ),
        catalog_entry(
            "openrouter",
            "OpenRouter",
            &[
                ("base_url", "Base URL"),
                ("site_url", "Site URL"),
                ("app_name", "App name"),
            ],
        ),
        catalog_entry("anthropic", "Anthropic", &[("base_url", "Base URL")]),
        catalog_entry("gemini", "Google Gemini", &[("base_url", "Base URL")]),
    ]
    .into_iter()
    .flatten()
    .collect();
    entries.extend([
        w::LlmProvider {
            id: agent_platform::ADAPTER.into(),
            name: "Google Agent Platform（旧 Vertex AI）".into(),
            adapter: agent_platform::ADAPTER.into(),
            endpoint: agent_platform::ENDPOINT.into(),
            settings: [
                ("project_id", "プロジェクト ID"),
                ("location", "ロケーション"),
            ]
            .into_iter()
            .map(|(key, label)| w::SettingField {
                key: key.into(),
                label: label.into(),
                kind: "string".into(),
                ..Default::default()
            })
            .collect(),
            requires_credentials: true,
            ..Default::default()
        },
        w::LlmProvider {
            id: "sakura".into(),
            name: "さくらのAI Engine".into(),
            adapter: "sakura".into(),
            endpoint: SAKURA_AI_ENDPOINT.into(),
            settings: vec![w::SettingField {
                key: "base_url".into(),
                label: "Base URL".into(),
                kind: "string".into(),
                ..Default::default()
            }],
            requires_credentials: true,
            environment_variable: Some(SAKURA_AI_API_KEY_ENV.into()),
            ..Default::default()
        },
    ]);
    entries
}

/// Every provider a connection can name: the built-ins no loaded plugin
/// shadows, then one entry per loaded plugin that serves LLM connections.
///
/// A package named like a built-in replaces that built-in, because a running
/// plugin already answers for the adapter name the connection carries.
pub(crate) fn providers(loaded: &Providers) -> Vec<w::LlmProvider> {
    let mut adapters: Vec<String> = loaded.adapters().collect();
    // The registry is a hash map; sorting keeps the catalogue the same on
    // every read, in this process and the next.
    adapters.sort_unstable();
    let plugins: Vec<w::LlmProvider> = adapters
        .into_iter()
        .filter_map(|adapter| plugin_entry(&adapter, loaded))
        .collect();
    let mut entries: Vec<w::LlmProvider> = builtin_providers()
        .into_iter()
        .filter(|builtin| {
            !plugins
                .iter()
                .any(|plugin| plugin.adapter == builtin.adapter)
        })
        .collect();
    entries.extend(plugins);
    entries
}

/// One loaded plugin in the contract's own shape, when it serves LLM
/// connections.
fn plugin_entry(adapter: &str, loaded: &Providers) -> Option<w::LlmProvider> {
    let plugin = loaded.get(adapter)?;
    let (_, provider) = plugin.entry(Extension::Llm)?;
    Some(w::LlmProvider {
        id: provider.id.clone(),
        name: plugin.package.manifest.name.clone(),
        adapter: adapter.into(),
        endpoint: provider.endpoint.clone(),
        settings: provider.settings.iter().map(setting_field).collect(),
        requires_credentials: !matches!(&provider.auth, AuthProfile::None),
        environment_variable: provider
            .environment_variable
            .clone()
            .filter(|name| !name.is_empty()),
        ..Default::default()
    })
}

/// Plugin ABI -> wire shape. `ene-plugin` owns the guest-facing type; this is
/// the only place the two meet.
fn setting_field(field: &ene_plugin::SettingField) -> w::SettingField {
    w::SettingField {
        key: field.key.clone(),
        label: field.label.clone(),
        kind: field.kind.clone(),
        required: field.required,
        choices: field.choices.clone(),
        ..Default::default()
    }
}

pub(crate) fn provider(adapter: &str, loaded: &Providers) -> Option<w::LlmProvider> {
    plugin_entry(adapter, loaded).or_else(|| builtin_provider(adapter))
}

fn builtin_provider(adapter: &str) -> Option<w::LlmProvider> {
    builtin_providers()
        .into_iter()
        .find(|provider| provider.adapter == adapter)
}

/// The endpoint a connection calls: its configured one without a trailing
/// slash, or the provider's default when the configuration leaves it empty.
pub(crate) fn endpoint_of(connection: &Connection, loaded: &Providers) -> Option<String> {
    let configured = connection.endpoint().endpoint.trim().trim_end_matches('/');
    if !configured.is_empty() {
        return Some(configured.to_owned());
    }
    provider(&connection.endpoint().adapter, loaded)
        .map(|provider| provider.endpoint)
        .filter(|endpoint| !endpoint.is_empty())
}
