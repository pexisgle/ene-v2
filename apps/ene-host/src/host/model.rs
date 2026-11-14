//! LLM connections: secrets, the model catalog and the rig connection an
//! agent runs on.
use super::db::{Result, Store};
use super::error::HostError;
use super::mind::{Companion, Role};
use super::net;
use super::settings::{Check, Connection, ConnectionConfig, ConnectionKind, CredentialState};
use bytes::Bytes;
use futures_util::StreamExt;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::operation::Completion;
use rig_core::providers::registry::{ConnectOptions, ProviderId};
use rig_core::serve::adapters::ModelAdapter;
use rig_core::serve::{Dispatch, ErasedHandler};
use rig_ecs::effects::Handler;
use rig_ecs::model::Connection as ModelConnection;
use rig_http::http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse};
use rig_http::wasm_compat::WasmCompatSend;
use rig_reqwest::ReqwestClient;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

const KEYRING_SERVICE: &str = "ene-host";
const MAX_MODELS: usize = 4_096;
const MAX_PROBE_BYTES: usize = 2 * 1024 * 1024;

/// How long a model request may take end to end, and how much of an answer the
/// host reads before treating it as hostile.
const MODEL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_MODEL_BYTES: usize = 8 * 1024 * 1024;

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

/// What authenticates a connection: a credential the user or the environment
/// supplied, or the placeholder a loopback endpoint accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Secret {
    Registered(String),
    Loopback,
}

impl Secret {
    pub(crate) fn value(&self) -> &str {
        match self {
            Self::Registered(secret) => secret,
            Self::Loopback => LOOPBACK_SECRET,
        }
    }
}

/// The placeholder a loopback endpoint accepts. It is never a real credential,
/// so it may travel over plain HTTP to the local server only.
const LOOPBACK_SECRET: &str = "ene-local";

/// The secret a connection authenticates with: the keyring cache first, then
/// the provider's environment variable, then the loopback placeholder. `None`
/// means the host holds no credential for it.
pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
) -> Option<Secret> {
    if let Some(secret) = credentials.and_then(|credentials| credentials.secret(&connection.id)) {
        return Some(Secret::Registered(secret));
    }
    if let Some(name) = connection.environment_variable()
        && let Ok(secret) = std::env::var(name)
        && !secret.is_empty()
    {
        return Some(Secret::Registered(secret));
    }
    net::loopback_endpoint(&connection.endpoint().endpoint).then_some(Secret::Loopback)
}

fn secret_for(
    connection: &Connection,
    credentials: &Credentials,
) -> std::result::Result<String, HostError> {
    match connection_secret(connection, Some(credentials)) {
        // A real credential only ever travels over TLS, or to a loopback
        // endpoint when the user deliberately points the connection at a
        // local server.
        Some(Secret::Registered(secret)) => {
            if net::secure_endpoint(&connection.endpoint().endpoint) {
                Ok(secret)
            } else {
                Err(HostError::unavailable(
                    "a credential is only sent to an https or loopback endpoint",
                ))
            }
        }
        Some(Secret::Loopback) => Ok(LOOPBACK_SECRET.to_owned()),
        None => Err(HostError::unavailable(
            "no credential is registered for this connection",
        )),
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
    let default =
        super::settings::provider(&endpoint.adapter).map_or_else(String::new, |p| p.endpoint);
    let base = super::settings::endpoint_of(&connection)
        .ok_or_else(|| HostError::unavailable("the connection has no endpoint to call"))?;
    let mut options = ConnectOptions::new()
        .api_key(secret)
        .http(Transport::new(&base)?);
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

/// The one transport every model call goes through. `rig-core` hands it each
/// request, so this is where the shared endpoint guard runs, the request is
/// pinned to the origin the connection was configured with, and the answer is
/// cut at the cap. The default `ReqwestClient` follows redirects and reads a
/// body of any size, so none of that would hold there.
#[derive(Clone)]
struct Transport {
    client: ReqwestClient,
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl Transport {
    /// Remembers the origin `base` names. The endpoint policy itself is not
    /// decided here: `admit` asks the shared guard, which also resolves the
    /// host and is the only place that can judge a domain.
    fn new(base: &str) -> Result<Self> {
        let url = url::Url::parse(base)
            .map_err(|_| HostError::unavailable("the model endpoint is not a URL"))?;
        let Some(host) = url.host_str() else {
            return Err(HostError::unavailable("the model endpoint has no host"));
        };
        let client = net::client(MODEL_TIMEOUT)
            .map_err(|error| HostError::unavailable(format!("the model transport: {error}")))?;
        Ok(Self {
            client: ReqwestClient::from(client),
            scheme: url.scheme().to_owned(),
            host: host.to_owned(),
            port: url.port_or_known_default(),
        })
    }

    /// Whether `uri` still addresses the configured origin without credentials
    /// embedded in it. A redirect or an adapter change cannot move it.
    fn permits(&self, uri: &http::Uri) -> bool {
        let Ok(url) = url::Url::parse(&uri.to_string()) else {
            return false;
        };
        url.scheme() == self.scheme
            && url.host_str() == Some(self.host.as_str())
            && url.port_or_known_default() == self.port
            && url.username().is_empty()
            && url.password().is_none()
    }

    /// Runs the endpoint guard and the origin pin before anything leaves. The
    /// guard is the shared `net::guard_endpoint`, so a domain that resolves to
    /// a private address is refused here exactly as it is for voice and
    /// embeddings. The reason is a plain string so the caller decides how it
    /// becomes a transport error.
    async fn admit(&self, uri: &http::Uri) -> std::result::Result<(), String> {
        net::guard_endpoint(&uri.to_string(), "model").await?;
        if !self.permits(uri) {
            return Err("the model request left the origin the connection was set to".to_owned());
        }
        Ok(())
    }
}

/// A refusal in the shape `rig-core` reads, carrying no URL and no secret.
fn refused(reason: &str) -> http_client::Error {
    http_client::Error::instance(std::io::Error::other(reason.to_owned()))
}

/// The cap every model answer shares, streaming or not.
const TOO_LARGE: &str = "the model answer is larger than the host reads";

fn within_cap(size: usize) -> bool {
    size <= MAX_MODEL_BYTES
}

/// A declared length over the cap is refused before the body is read at all.
fn declared_too_large(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_MODEL_BYTES as u64)
}

impl HttpClientExt for Transport {
    fn send<T, U>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            this.admit(request.uri())
                .await
                .map_err(|reason| refused(&reason))?;
            let response = this.client.send_streaming(request).await?;
            let (parts, mut body) = response.into_parts();
            if declared_too_large(&parts.headers) {
                return Err(refused(TOO_LARGE));
            }
            let mut collected = Vec::new();
            while let Some(chunk) = body.next().await {
                let bytes = chunk?;
                // Refuse before the copy, so a hostile chunk never lands in
                // the buffer the cap is there to bound.
                if !within_cap(collected.len().saturating_add(bytes.len())) {
                    return Err(refused(TOO_LARGE));
                }
                collected.extend_from_slice(&bytes);
            }
            let value = U::from(Bytes::from(collected));
            let lazy: LazyBody<U> = Box::pin(async move { Ok(value) });
            Ok(http::Response::from_parts(parts, lazy))
        }
    }

    /// No provider this host talks to takes a multipart completion request, so
    /// there is no body to shape and the guard has nothing to admit.
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _: http::Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        async { Err(refused("the host sends no multipart model request")) }
    }

    fn send_streaming<T>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            this.admit(request.uri())
                .await
                .map_err(|reason| refused(&reason))?;
            let response = this.client.send_streaming(request).await?;
            let (parts, mut body) = response.into_parts();
            if declared_too_large(&parts.headers) {
                return Err(refused(TOO_LARGE));
            }
            let bounded = async_stream::try_stream! {
                let mut size = 0usize;
                while let Some(chunk) = body.next().await {
                    let bytes = chunk?;
                    size = size.saturating_add(bytes.len());
                    if !within_cap(size) {
                        Err(refused(TOO_LARGE))?;
                    }
                    yield bytes;
                }
            };
            Ok(http::Response::from_parts(
                parts,
                Box::pin(bounded) as http_client::BoxedStream,
            ))
        }
    }
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
    let Ok(secret) = secret_for(connection, credentials) else {
        return failure("no credential is registered for this connection".into());
    };
    // A connection that leaves its endpoint empty calls the provider's own
    // default, which is exactly what the probe has to ask.
    let Some(base) = super::settings::endpoint_of(connection) else {
        return failure("the connection has no endpoint to probe".into());
    };
    let url = format!("{base}/models");
    if let Err(error) = net::guard_endpoint(&url, "connection").await {
        return failure(error);
    }
    let client = match net::client(std::time::Duration::from_secs(15)) {
        Ok(client) => client,
        Err(error) => return failure(error),
    };
    let bytes = match client
        .get(&url)
        .bearer_auth(&secret)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            match net::read_body(response, MAX_PROBE_BYTES, "connection").await {
                Ok(bytes) => bytes,
                Err(error) => return failure(error),
            }
        }
        Ok(response) => return failure(format!("endpoint answered {}", response.status())),
        Err(error) => return failure(format!("endpoint unreachable: {error}")),
    };
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

    #[test]
    fn a_real_secret_never_travels_over_plain_http_to_a_remote_endpoint() {
        let credentials = Credentials::default();
        credentials.remember("c1", "secret".into());
        assert_eq!(
            secret_for(&connection_at("https://api.example.com/v1"), &credentials).unwrap(),
            "secret"
        );
        assert_eq!(
            secret_for(&connection_at("http://127.0.0.1:11434/v1"), &credentials).unwrap(),
            "secret"
        );
        assert_eq!(
            secret_for(&connection_at("http://localhost:11434/v1"), &credentials).unwrap(),
            "secret"
        );
        assert!(secret_for(&connection_at("http://api.example.com/v1"), &credentials).is_err());
        // The loopback dummy key still needs no real credential, and a remote
        // plain-http endpoint never gets one either way.
        let anonymous = Credentials::default();
        assert_eq!(
            secret_for(&connection_at("http://127.0.0.1:11434/v1"), &anonymous).unwrap(),
            "ene-local"
        );
        assert!(secret_for(&connection_at("http://api.example.com/v1"), &anonymous).is_err());
    }

    /// The transport is the only place a model request can be redirected or
    /// pointed at a private address, so both refusals have to hold here. The
    /// cases that need no name lookup run without touching the network.
    #[tokio::test]
    async fn the_model_transport_stays_on_its_origin_and_off_private_addresses() {
        let transport = Transport::new("https://api.openai.com/v1").unwrap();
        assert!(transport.permits(&"https://api.openai.com/v1/responses".parse().unwrap()));
        for other in [
            "https://api.example.com/v1/responses",
            "http://api.openai.com/v1/responses",
            "https://api.openai.com:8443/v1/responses",
        ] {
            assert!(!transport.permits(&other.parse().unwrap()), "{other}");
        }

        // Plain remote HTTP and a literal private address are refused before
        // the request is built, and loopback is the one plain-HTTP exception.
        for refused in [
            "http://api.example.com/v1/responses",
            "https://169.254.169.254/v1/responses",
            "https://[fe80::1]/v1/responses",
            "ftp://127.0.0.1/v1/responses",
        ] {
            let uri = refused.parse().unwrap();
            assert!(transport.admit(&uri).await.is_err(), "{refused}");
        }
        let local = Transport::new("http://127.0.0.1:11434/v1").unwrap();
        assert!(
            local
                .admit(&"http://127.0.0.1:11434/v1/models".parse().unwrap())
                .await
                .is_ok()
        );
        // The guard also rejects an origin the transport is not pinned to.
        assert!(
            local
                .admit(&"http://127.0.0.1:11434/other".parse().unwrap())
                .await
                .is_ok()
        );
        assert!(
            local
                .admit(&"http://127.0.0.1:9999/v1".parse().unwrap())
                .await
                .is_err()
        );
    }
}
