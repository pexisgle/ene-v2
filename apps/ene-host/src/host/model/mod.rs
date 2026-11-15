//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
mod plugin;

use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::net;
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind, CredentialState};
pub(crate) use plugin::completion as plugin_completion;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use rig_core::providers::{
    openai::OpenAIConfig,
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

    pub(crate) fn secret(&self, connection: &str) -> Option<String> {
        if self.session_only {
            self.session_secrets().get(connection).cloned()
        } else {
            read_keyring(connection).ok().flatten()
        }
    }

    pub(crate) fn session_only(&self) -> bool {
        self.session_only
    }
}

fn keyring_entry(connection: &str) -> std::result::Result<keyring::Entry, HostError> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("connection:{connection}"))
        .map_err(|error| HostError::unavailable(format!("credential store: {error}")))
}

fn read_keyring(connection: &str) -> std::result::Result<Option<String>, HostError> {
    match keyring_entry(connection)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(HostError::unavailable(format!("credential store: {error}"))),
    }
}

/// Stores a secret in the keyring (or only in memory for a session-only host).
pub(crate) fn save_secret(credentials: &Credentials, connection: &str, secret: &str) -> Result<()> {
    if secret.is_empty() || secret.chars().count() > 8_192 {
        return Err(HostError::invalid("credential must be 1-8192 characters"));
    }
    if !credentials.session_only() {
        keyring_entry(connection)?
            .set_password(secret)
            .map_err(|error| HostError::unavailable(format!("credential store: {error}")))?;
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
            Err(error) => return Err(HostError::unavailable(format!("credential store: {error}"))),
        }
    }
    credentials.forget(connection);
    Ok(())
}

/// The state the contract reports for a connection's secret.
pub(crate) fn credential_state(
    connection: &Connection,
    credentials: &Credentials,
) -> CredentialState {
    if credentials.secret(&connection.id).is_some() {
        return CredentialState::Registered;
    }
    if connection
        .environment_variable()
        .is_some_and(|name| std::env::var_os(name).is_some())
    {
        return CredentialState::Environment;
    }
    CredentialState::Absent
}

pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
) -> Option<String> {
    if let Some(secret) = credentials.and_then(|credentials| credentials.secret(&connection.id)) {
        return Some(secret);
    }
    let name = connection.environment_variable()?;
    std::env::var(name).ok().filter(|secret| !secret.is_empty())
}

/// The rig provider a host adapter names.
///
/// The host keeps its own adapter names: its `gemini` connection is rig's
/// `gcp.gemini` provider. Every other adapter is spelled the same in both.
pub(crate) fn provider_of(adapter: &str) -> Option<ProviderId> {
    ProviderId::catalog(match adapter {
        "gemini" => "gcp.gemini",
        adapter => adapter,
    })
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
        .ok_or_else(|| HostError::unavailable("no model is assigned to this role"))?;
    let connection = store
        .connection(&assignment.connection_id)?
        .ok_or_else(|| HostError::unavailable("the assigned connection does not exist"))?;
    if !connection.enabled {
        return Err(HostError::unavailable(
            "the assigned connection is disabled",
        ));
    }
    if connection.kind != ConnectionKind::Llm {
        return Err(HostError::unavailable(
            "the assigned connection is not an LLM connection",
        ));
    }
    let endpoint = connection.endpoint();
    // A plugin that serves this connection's adapter answers instead of the
    // catalog, so a provider rig does not ship needs no host change.
    if let Some(running) = providers.running(&endpoint.adapter, ene_plugin::Extension::Llm) {
        let grant =
            super::providers::Grant::new(running.provider(), &connection, Some(credentials));
        let broker = Arc::new(ene_plugin_runtime::service::HttpBroker::new(Arc::new(
            grant,
        )));
        return Ok(plugin_completion(
            running,
            assignment.model_id.clone(),
            broker,
        ));
    }
    let provider = provider_of(&endpoint.adapter).ok_or_else(|| {
        HostError::unavailable(format!("unsupported adapter `{}`", endpoint.adapter))
    })?;
    // The adapter's label names the connection; the catalog files the model
    // under the provider's rig name, which differs for `gemini`.
    let catalog_reference = format!("{}/{}", provider.vendor(), assignment.model_id);
    let spec = match catalog.resolve(&catalog_reference) {
        Ok(resolved) => resolved.shared(),
        Err(_) => {
            let mut spec = ModelSpec::new(provider, assignment.model_id.clone());
            // An unlisted model is assumed to call tools; the host cannot
            // prove otherwise without a probe.
            spec.tools = true;
            Arc::new(spec)
        }
    };
    let secret = connection_secret(&connection, Some(credentials));
    let default =
        super::settings::provider(&endpoint.adapter).map_or_else(String::new, |p| p.endpoint);
    let base = super::settings::endpoint_of(&connection)
        .ok_or_else(|| HostError::unavailable("the connection has no endpoint to call"))?;
    // Rig builds the model requests, so their deadline sits on the model
    // client; every other host request shares the unbounded one.
    let client = net::model_client().map_err(HostError::unavailable)?;
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
        .map_err(|error| HostError::unavailable(format!("model connection: {error}")))?;
    Ok(model)
}

/// Lists the models a connection declares, with whatever the catalog knows.
pub(crate) fn models(connection: &Connection) -> Vec<(String, ModelSpec)> {
    let ConnectionConfig::Llm(config) = &connection.config else {
        return Vec::new();
    };
    let catalog = Catalog::builtin();
    config
        .model_ids
        .iter()
        .take(MAX_MODELS)
        .map(|model| {
            let provider = provider_of(&connection.endpoint().adapter)
                .unwrap_or_else(|| ProviderId::catalog("openai").expect("openai is registered"));
            let reference = format!("{}/{}", provider.vendor(), model);
            let spec = catalog
                .resolve(&reference)
                .map(|resolved| resolved.spec.clone())
                .unwrap_or_else(|_| ModelSpec::new(provider, model.clone()));
            (model.clone(), spec)
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
    let secret = connection_secret(connection, Some(credentials));
    // A connection that leaves its endpoint empty calls the provider's own
    // default, which is exactly what the probe has to ask.
    let Some(base) = super::settings::endpoint_of(connection) else {
        return failure("the connection has no endpoint to probe".into());
    };
    let listed = match list_models(connection, &base, secret, providers).await {
        Ok(models) => models,
        Err(error) => return failure(error),
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
        capabilities: vec!["streaming_text".into(), "model_list".into()],
        resources,
    }
}

/// Asks the provider's own model-listing endpoint, through rig's provider
/// clients, so the probe reads the wire the connection is configured for
/// instead of assuming an OpenAI-shaped `/models` reply.
async fn list_models(
    connection: &Connection,
    base: &str,
    secret: Option<String>,
    providers: &super::providers::Providers,
) -> std::result::Result<Vec<String>, String> {
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin lists its own models, through the same `ListModels` operation
    // the agent loop would use, so the probe reads what the plugin serves.
    if let Some(running) = providers.running(adapter, ene_plugin::Extension::Llm) {
        return plugin_models(running, base).await;
    }
    let secret = secret.unwrap_or_default();
    // The host's adapter list is not rig's provider list: `gemini` is
    // `gcp.gemini` there, and an adapter the registry does not know has no
    // preset at all. Those keep the OpenAI-compatible `/models` wire the host
    // has always probed, pinned to the connection's endpoint.
    let default = super::settings::provider(adapter).map_or_else(String::new, |p| p.endpoint);
    let config = match provider_of(adapter).and_then(|p| p.config(secret.clone())) {
        Some(config) if base != default => config.with_base_url(base),
        Some(config) => config,
        None => ProviderConfig::OpenAi(OpenAIConfig::new(secret).with_base_url(base)),
    };
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
    running: std::sync::Arc<super::providers::Running>,
    base: &str,
) -> std::result::Result<Vec<String>, String> {
    let (sender, mut events) = tokio::sync::mpsc::channel(16);
    let provider = running.provider().id.clone();
    let endpoint = base.to_owned();
    let options = running.provider().options.clone();
    // A model list is control-plane: the plugin may name models without
    // reaching the network, so the broker denies any exchange it attempts.
    let services: std::sync::Arc<dyn ene_plugin_runtime::instance::Services> =
        super::providers::BrokerServices::new(
            std::sync::Arc::new(ene_plugin_runtime::service::HttpBroker::new(
                std::sync::Arc::new(ene_plugin_runtime::service::DenyHttp),
            )),
            provider,
        );
    let collected = tokio::time::timeout(PROBE_TIMEOUT, async move {
        let mut models = Vec::new();
        let call = tokio::spawn(async move {
            running
                .invoke_streaming(
                    ene_plugin::Operation::ListModels { endpoint, options },
                    services,
                    sender,
                )
                .await
        });
        while let Some(event) = events.recv().await {
            if let ene_plugin::Event::Models { models: listed } = event {
                models = listed.into_iter().map(|model| model.id).collect();
            }
        }
        let _ = call.await;
        models
    })
    .await
    .map_err(|_| format!("the plugin did not answer in {}s", PROBE_TIMEOUT.as_secs()))?;
    Ok(collected)
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

    /// Serves the model listing the request path asks for: Gemini's native
    /// reply under `/v1beta/models`, the OpenAI-shaped one elsewhere.
    async fn models_endpoint() -> std::net::SocketAddr {
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
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body = if path.starts_with("/v1beta/models") {
                    r#"{"models":[{"name":"models/gemini-probe"}]}"#
                } else {
                    r#"{"data":[{"id":"probe-model"}]}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        address
    }

    /// The probe lists models through the configured endpoint whichever
    /// adapter names it: `openai` and `openrouter` have registry presets,
    /// `gemini` maps to rig's `gcp.gemini` provider and reads its native
    /// listing, and a custom adapter falls back to the OpenAI-shaped wire.
    #[tokio::test]
    async fn the_probe_lists_models_for_any_adapter() {
        let address = models_endpoint().await;
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        for (adapter, model) in [
            ("openai", "probe-model"),
            ("openrouter", "probe-model"),
            ("gemini", "gemini-probe"),
            ("custom", "probe-model"),
        ] {
            let endpoint = format!("http://{address}");
            let connection = connection_for(adapter, &endpoint);
            let check = probe(
                &store,
                &Credentials::default(),
                &connection,
                &super::super::providers::Providers::none(),
            )
            .await;
            assert!(check.healthy, "{adapter}: {:?}", check.failure);
            assert!(
                check.resources.iter().any(|known| known == model),
                "{adapter}: {:?}",
                check.resources
            );
        }
    }
}
