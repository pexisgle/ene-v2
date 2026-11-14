//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::net;
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind, CredentialState};
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::operation::Completion;
use rig_core::providers::registry::{ConnectOptions, ProviderConfig, ProviderId};
use rig_core::serve::adapters::ModelAdapter;
use rig_core::serve::{Dispatch, ErasedHandler};
use rig_ecs::effects::Handler;
use rig_ecs::model::Connection as ModelConnection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

const KEYRING_SERVICE: &str = "ene-host";
const MAX_MODELS: usize = 4_096;

/// How long the model-list probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Secrets stay out of the state database: the OS keyring holds them and this
/// cache only mirrors what the host thread needs synchronously.
#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Credentials {
    cache: Arc<Mutex<HashMap<String, String>>>,
    session_only: bool,
}

impl Credentials {
    /// Reads the keyring once at startup unless the session-only mode is set.
    pub(crate) fn load(store: &Store, session_only: bool) -> Self {
        let credentials = Self {
            cache: Arc::default(),
            session_only,
        };
        if session_only {
            return credentials;
        }
        let Ok(settings) = store.settings() else {
            return credentials;
        };
        for connection in settings
            .connections
            .iter()
            .filter(|connection| connection.credential_state == CredentialState::Registered)
        {
            if let Ok(Some(secret)) = read_keyring(&connection.id) {
                credentials.remember(&connection.id, secret);
            }
        }
        credentials
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, String>> {
        super::lock(&self.cache)
    }

    pub(crate) fn remember(&self, connection: &str, secret: String) {
        self.cache().insert(connection.to_owned(), secret);
    }

    pub(crate) fn forget(&self, connection: &str) {
        self.cache().remove(connection);
    }

    pub(crate) fn secret(&self, connection: &str) -> Option<String> {
        self.cache().get(connection).cloned()
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
    credentials.remember(connection, secret.to_owned());
    Ok(())
}

pub(crate) fn delete_secret(credentials: &Credentials, connection: &str) -> Result<()> {
    credentials.forget(connection);
    if credentials.session_only() {
        return Ok(());
    }
    match keyring_entry(connection)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(HostError::unavailable(format!("credential store: {error}"))),
    }
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

/// The secret a connection authenticates with: the keyring cache first, then
/// the provider's environment variable. `None` means the host holds none, and
/// the request goes out without one: a local server that takes any key is the
/// operator's own choice.
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

/// The model an agent runs on.
pub(crate) struct Bound {
    pub(crate) connection: ModelConnection,
}

/// Builds the rig model for one role of a companion.
pub(crate) fn connect(
    store: &Store,
    credentials: &Credentials,
    catalog: &Catalog,
    companion: &Companion,
    role: Role,
) -> Result<Bound> {
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
    let provider = ProviderId::catalog(&endpoint.adapter).ok_or_else(|| {
        HostError::unavailable(format!("unsupported adapter `{}`", endpoint.adapter))
    })?;
    let reference = format!("{}/{}", endpoint.adapter, assignment.model_id);
    let spec = match catalog.resolve(&reference) {
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
    let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(reference.clone(), model));
    let connection = ModelConnection {
        spec: Arc::clone(&spec),
        handler: Handler(handler),
    };
    Ok(Bound { connection })
}

/// Asks a bound connection one unary completion.
///
/// The call runs outside the `AgentPass` lifecycle: no turn scheduled it, so
/// the effect id is synthetic and the caller bounds it with its own task
/// handle. Summaries, consolidation and the heartbeat tick all read one reply
/// and stop, which is why they cannot wait for a pass to settle.
pub(crate) async fn unary(
    handler: &ErasedHandler,
    request: CompletionRequest,
) -> std::result::Result<std::sync::Arc<CompletionResponse>, String> {
    let kind = EffectKind::Completion {
        request: Arc::new(request),
        stream: false,
    };
    let dispatch = Dispatch::new(EffectId::from_raw(0), false);
    match handler.handle(kind, dispatch).await.into_outcome().await {
        Ok(Answered::Completion(response)) => Ok(response),
        Ok(_) => Err("the connection answered something other than a completion".to_owned()),
        Err(report) => Err(report.to_string()),
    }
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
            let reference = format!("{}/{}", connection.endpoint().adapter, model);
            let spec = catalog
                .resolve(&reference)
                .map(|resolved| resolved.spec.clone())
                .unwrap_or_else(|_| {
                    ModelSpec::new(
                        ProviderId::catalog(&connection.endpoint().adapter).unwrap_or_else(|| {
                            ProviderId::catalog("openai").expect("openai is registered")
                        }),
                        model.clone(),
                    )
                });
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
    let listed = match list_models(connection, &base, secret).await {
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
) -> std::result::Result<Vec<String>, String> {
    let adapter = connection.endpoint().adapter.as_str();
    let provider =
        ProviderId::catalog(adapter).ok_or_else(|| format!("unsupported adapter `{adapter}`"))?;
    let config = provider
        .config(secret.unwrap_or_default())
        .ok_or_else(|| format!("`{adapter}` has no configurable provider"))?;
    let http = rig_reqwest::ReqwestClient::from(net::client()?);
    let config = config.with_base_url(base);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{Endpoint, LlmConnection};

    fn connection_at(endpoint: &str) -> Connection {
        Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openai".into(),
                    endpoint: endpoint.into(),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_ids: vec!["gpt-5-mini".into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    fn connection() -> Connection {
        connection_at("https://api.openai.com/v1")
    }

    #[test]
    fn a_connection_without_a_secret_reports_none() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::default();
        let connection = connection();
        assert_eq!(
            credential_state(&connection, &credentials),
            CredentialState::Absent
        );
        assert!(connection_secret(&connection, Some(&credentials)).is_none());
        credentials.remember("c1", "secret".into());
        assert_eq!(
            credential_state(&connection, &credentials),
            CredentialState::Registered
        );
        assert_eq!(
            connection_secret(&connection, Some(&credentials)).as_deref(),
            Some("secret")
        );
        let models = models(&connection);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "gpt-5-mini");
        let _ = store;
    }

    /// The secret comes from the keyring or the provider's environment, and a
    /// connection with either is what `credential_state` reports.
    #[test]
    fn a_secret_is_read_from_the_cache_before_the_environment() {
        let credentials = Credentials::default();
        let remote = connection_at("https://api.example.com/v1");
        assert_eq!(
            connection_secret(&remote, Some(&credentials)),
            None,
            "no credential and no environment variable"
        );
        credentials.remember("c1", "cached".into());
        assert_eq!(
            connection_secret(&remote, Some(&credentials)).as_deref(),
            Some("cached")
        );
    }
}
