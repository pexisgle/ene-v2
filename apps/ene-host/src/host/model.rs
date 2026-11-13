//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind, CredentialState};
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::operation::Completion;
use rig_core::providers::registry::{ConnectOptions, ProviderId};
use rig_core::serve::ErasedHandler;
use rig_core::serve::adapters::ModelAdapter;
use rig_ecs::effects::Handler;
use rig_ecs::model::Connection as ModelConnection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

const KEYRING_SERVICE: &str = "ene-host";
const MAX_MODELS: usize = 4_096;
const MAX_PROBE_BYTES: usize = 2 * 1024 * 1024;

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
        self.cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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

fn secret_for(
    connection: &Connection,
    credentials: &Credentials,
) -> std::result::Result<String, HostError> {
    if let Some(secret) = credentials.secret(&connection.id) {
        return Ok(secret);
    }
    if let Some(name) = connection.environment_variable()
        && let Ok(secret) = std::env::var(name)
        && !secret.is_empty()
    {
        return Ok(secret);
    }
    // A loopback endpoint is a local server; it may accept any key and the
    // host never sends a real secret to a non-loopback endpoint without one.
    if loopback_endpoint(&connection.endpoint().endpoint) {
        return Ok("ene-local".into());
    }
    Err(HostError::unavailable(
        "no credential is registered for this connection",
    ))
}

fn loopback_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
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
    let secret = secret_for(&connection, credentials)?;
    let mut options = ConnectOptions::new()
        .api_key(secret)
        .http(rig_reqwest::ReqwestClient::default());
    if !endpoint.endpoint.trim().is_empty()
        && endpoint.endpoint != connection_provider_endpoint(&endpoint.adapter)
    {
        options = options.base_url(endpoint.endpoint.clone());
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

fn connection_provider_endpoint(adapter: &str) -> String {
    super::settings::provider(adapter).map_or_else(String::new, |provider| provider.endpoint)
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
    if config.model_ids.is_empty() && connection.endpoint().endpoint.is_empty() {
        return failure("the connection has no models and no endpoint".into());
    }
    let Ok(secret) = secret_for(connection, credentials) else {
        return failure("no credential is registered for this connection".into());
    };
    let base = connection.endpoint().endpoint.trim_end_matches('/');
    if base.is_empty() {
        return failure("the connection has no endpoint to probe".into());
    }
    let url = format!("{base}/models");
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => return failure(format!("http client: {error}")),
    };
    let bytes = match client
        .get(&url)
        .bearer_auth(&secret)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return failure(format!("endpoint body: {error}")),
        },
        Ok(response) => return failure(format!("endpoint answered {}", response.status())),
        Err(error) => return failure(format!("endpoint unreachable: {error}")),
    };
    if bytes.len() > MAX_PROBE_BYTES {
        return failure("endpoint answered more data than the host reads".into());
    }
    let mut resources: Vec<String> = config.model_ids.clone();
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(data) = value.get("data").and_then(serde_json::Value::as_array)
    {
        for model in data.iter().filter_map(|model| model.get("id")?.as_str()) {
            if resources.len() >= MAX_MODELS {
                break;
            }
            if !resources.iter().any(|known| known == model) {
                resources.push(model.to_owned());
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{Endpoint, LlmConnection};

    fn connection() -> Connection {
        Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openai".into(),
                    endpoint: "https://api.openai.com/v1".into(),
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

    #[test]
    fn a_connection_without_a_secret_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let credentials = Credentials::default();
        let connection = connection();
        assert_eq!(
            credential_state(&connection, &credentials),
            CredentialState::Absent
        );
        assert!(secret_for(&connection, &credentials).is_err());
        credentials.remember("c1", "secret".into());
        assert_eq!(
            credential_state(&connection, &credentials),
            CredentialState::Registered
        );
        assert!(secret_for(&connection, &credentials).is_ok());
        let models = models(&connection);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "gpt-5-mini");
        let _ = store;
    }
}
