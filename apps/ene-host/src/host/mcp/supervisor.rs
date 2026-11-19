//! MCP connection supervisor: settings/credential diffing, backoff, auth
//! injection, stdio environment/cwd application, and background connection
//! lifecycle management.
use super::projection::{attach, project_servers, retire_one};
use crate::host::{
    db::Store,
    model::Credentials,
    providers::Providers,
    resources::HostRuntime,
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use rig_ecs::prelude::*;
use rig_rmcp::{
    McpClientHandler, McpConnection,
    rmcp::{
        model::ClientConfig,
        transport::{
            child_process::TokioChildProcess,
            streamable_http_client::{
                StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
            },
        },
    },
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

/// How many connect results may queue before a producer waits. A connect
/// happens once per server, so this is a bound, not a throughput target.
const QUEUE: usize = 32;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Launch {
    pub(super) revision: u64,
    pub(super) credential_generation: Option<u64>,
    pub(super) owner: String,
}

/// One connected server and the tool entities it owns.
pub(super) struct Server {
    pub(super) connection: McpConnection,
    pub(super) launch: Launch,
    pub(super) registry: ToolServerHandle,
    pub(super) label: String,
    pub(super) allowed: Vec<String>,
}

/// The servers that are up right now, keyed by connection id.
#[derive(Resource, Default)]
pub(crate) struct McpServers(pub(super) HashMap<String, Server>);

/// Connection ids a connect task is running for.
#[derive(Resource, Default)]
pub(super) struct Connecting(pub(super) HashMap<String, Pending>);

pub(super) struct Pending {
    pub(super) launch: Launch,
    pub(super) _task: AbortOnDropHandle<()>,
}

pub(super) enum FailureKind {
    Transient {
        attempts: u32,
        next_retry: Instant,
        _wake_timer: Option<AbortOnDropHandle<()>>,
    },
    ConfigHold,
}

pub(super) struct FailureState {
    pub(super) revision: u64,
    pub(super) credential_generation: Option<u64>,
    pub(super) reason: String,
    pub(super) kind: FailureKind,
}

/// Backoff and configuration-hold tracking per failing MCP connection.
#[derive(Resource, Default)]
pub(super) struct McpFailures(pub(super) HashMap<String, FailureState>);

pub(super) enum Event {
    Connected {
        id: String,
        launch: Launch,
        connection: McpConnection,
        registry: ToolServerHandle,
    },
    Failed {
        id: String,
        launch: Launch,
        reason: String,
        config_error: bool,
    },
}

#[derive(Resource)]
pub(super) struct McpEvents {
    pub(super) sender: mpsc::Sender<Event>,
    pub(super) receiver: mpsc::Receiver<Event>,
}

impl Default for McpEvents {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self { sender, receiver }
    }
}

/// What one settings row asks the host to connect to.
#[derive(Clone)]
pub(super) struct Desired {
    pub(super) revision: u64,
    pub(super) credential_generation: Option<u64>,
    pub(super) name: String,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) cwd: String,
    pub(super) env: BTreeMap<String, String>,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) auth_header: String,
    pub(super) secret_env_var: String,
    pub(super) endpoint: String,
    pub(super) secret: Option<String>,
    pub(super) allowed: Vec<String>,
}

#[derive(Resource, Default)]
pub(super) struct McpSettings {
    pub(super) revision: Option<u64>,
    pub(super) credential_generation: Option<u64>,
    pub(super) desired: Vec<(String, Desired)>,
}

/// Live inspection state returned to RPC connection probes.
pub(crate) enum CheckState {
    Connected(Vec<String>),
    Failed(String),
    Connecting,
}

#[derive(Debug)]
pub(super) struct ConnectFailure {
    pub(super) reason: String,
    pub(super) config_error: bool,
}

fn desired(
    store: &Store,
    credentials: Option<&Credentials>,
    credential_generation: Option<u64>,
) -> crate::host::db::Result<Vec<(String, Desired)>> {
    let settings = store.settings()?;
    let no_providers = Providers::none();
    Ok(settings
        .connections
        .iter()
        .filter(|connection| connection.enabled && connection.kind == ConnectionKind::Mcp)
        .filter_map(|connection| {
            let ConnectionConfig::Mcp(config) = &connection.config else {
                return None;
            };
            let secret =
                crate::host::model::connection_secret(connection, credentials, &no_providers)
                    .ok()
                    .flatten();
            Some((
                connection.id.clone(),
                Desired {
                    revision: connection.revision,
                    credential_generation,
                    name: connection.name.clone(),
                    command: config.command.trim().to_owned(),
                    args: config.args.clone(),
                    cwd: config.cwd.trim().to_owned(),
                    env: config.env.clone(),
                    headers: config.headers.clone(),
                    auth_header: config.auth_header.trim().to_owned(),
                    secret_env_var: config.secret_env_var.trim().to_owned(),
                    endpoint: config.endpoint.endpoint.trim().to_owned(),
                    secret,
                    allowed: config.allowed_tools.clone(),
                },
            ))
        })
        .collect())
}

/// Connects what the settings ask for, retires what they no longer do, and
/// attaches the tools a handler listed.
pub(super) fn sync(world: &mut World) {
    let closing = world
        .get_resource::<crate::host::resources::HostState>()
        .is_some_and(|state| state.closing || state.failed.is_some());
    if closing {
        world.resource_mut::<Connecting>().0.clear();
        world.resource_mut::<McpFailures>().0.clear();
        let ids: Vec<String> = world.resource::<McpServers>().0.keys().cloned().collect();
        for id in ids {
            retire_one(world, &id);
        }
        if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
            for event in drain(world) {
                if let Event::Connected { connection, .. } = event {
                    runtime.spawn(async move { connection.close().await });
                }
            }
        }
        return;
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) else {
        return;
    };
    let store = world.resource::<Store>().clone();
    let revision = match store.settings_revision() {
        Ok(revision) => revision,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            return;
        }
    };
    let credentials = world.get_resource::<Credentials>().cloned();
    let credential_generation = credentials.as_ref().map(Credentials::generation);
    let needs_reload = {
        let current = world.resource::<McpSettings>();
        current.revision != Some(revision) || current.credential_generation != credential_generation
    };
    if needs_reload {
        let desired = match desired(&store, credentials.as_ref(), credential_generation) {
            Ok(desired) => desired,
            Err(error) => {
                if error.is_storage() {
                    crate::host::fail(world, error.to_string());
                }
                return;
            }
        };
        *world.resource_mut::<McpSettings>() = McpSettings {
            revision: Some(revision),
            credential_generation,
            desired,
        };
    }
    let wanted = world.resource::<McpSettings>().desired.clone();

    // Cancel pending connect tasks whose revision or credentials changed.
    world.resource_mut::<Connecting>().0.retain(|id, pending| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == pending.launch.revision
                && want.credential_generation == pending.launch.credential_generation
        })
    });

    // Clear failure holds/backoffs when connection config or credentials changed, or connection left wanted.
    world.resource_mut::<McpFailures>().0.retain(|id, failure| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == failure.revision
                && want.credential_generation == failure.credential_generation
        })
    });

    for event in drain(world) {
        accept_event(world, &wanted, event, &runtime);
    }

    let wanted_ids: HashSet<&str> = wanted.iter().map(|(id, _)| id.as_str()).collect();
    let retire: Vec<String> = world
        .resource::<McpServers>()
        .0
        .iter()
        .filter(|(id, server)| {
            server.connection.is_closed()
                || !wanted_ids.contains(id.as_str())
                || wanted
                    .iter()
                    .find(|(wanted, _)| wanted == *id)
                    .is_some_and(|(_, want)| {
                        want.revision != server.launch.revision
                            || want.credential_generation != server.launch.credential_generation
                    })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in retire {
        retire_one(world, &id);
    }

    project_servers(world);

    let now = Instant::now();
    let sender = world.resource::<McpEvents>().sender.clone();
    let wake = world.get_resource::<Wake>().cloned();
    for (id, want) in wanted {
        if world.resource::<McpServers>().0.contains_key(&id)
            || world.resource::<Connecting>().0.contains_key(&id)
        {
            continue;
        }
        if let Some(failure) = world.resource::<McpFailures>().0.get(&id) {
            match &failure.kind {
                FailureKind::ConfigHold => continue,
                FailureKind::Transient { next_retry, .. } if now < *next_retry => continue,
                FailureKind::Transient { .. } => {}
            }
        }
        let registry = ToolServer::new().run();
        let launch = Launch {
            revision: want.revision,
            credential_generation: want.credential_generation,
            owner: registry.owner(),
        };
        let event_id = id.clone();
        let event_launch = launch.clone();
        let sender = sender.clone();
        let wake = wake.clone();
        let task = runtime.spawn(async move {
            let event = match open(&want, registry.clone()).await {
                Ok(connection) => Event::Connected {
                    id: event_id,
                    launch: event_launch,
                    connection,
                    registry,
                },
                Err(failure) => Event::Failed {
                    id: event_id,
                    launch: event_launch,
                    reason: failure.reason,
                    config_error: failure.config_error,
                },
            };
            match sender.send(event).await {
                Ok(()) => {
                    if let Some(wake) = wake {
                        wake.wake();
                    }
                }
                Err(error) => {
                    if let Event::Connected { connection, .. } = error.0 {
                        connection.close().await;
                    }
                }
            }
        });
        world.resource_mut::<Connecting>().0.insert(
            id,
            Pending {
                launch,
                _task: AbortOnDropHandle::new(task),
            },
        );
    }
}

pub(super) fn accept_event(
    world: &mut World,
    wanted: &[(String, Desired)],
    event: Event,
    runtime: &tokio::runtime::Handle,
) {
    let (id, launch) = match &event {
        Event::Connected { id, launch, .. } | Event::Failed { id, launch, .. } => (id, launch),
    };
    let current = world
        .resource::<Connecting>()
        .0
        .get(id)
        .is_some_and(|pending| &pending.launch == launch)
        && wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == launch.revision
                && want.credential_generation == launch.credential_generation
        });
    if !current {
        if let Event::Connected { connection, .. } = event {
            runtime.spawn(async move { connection.close().await });
        }
        return;
    }
    world.resource_mut::<Connecting>().0.remove(id);
    match event {
        Event::Connected {
            id,
            launch,
            connection,
            registry,
        } => {
            world.resource_mut::<McpFailures>().0.remove(&id);
            let want = &wanted.iter().find(|(wanted, _)| wanted == &id).unwrap().1;
            attach(world, &id, launch, connection, registry, want);
        }
        Event::Failed {
            id,
            launch,
            reason,
            config_error,
        } => {
            tracing::warn!(connection = %id, config_error, "the MCP server did not connect: {reason}");
            record_failure(world, id, launch, reason, config_error, runtime);
        }
    }
}

fn record_failure(
    world: &mut World,
    id: String,
    launch: Launch,
    reason: String,
    config_error: bool,
    runtime: &tokio::runtime::Handle,
) {
    let wake = world.get_resource::<Wake>().cloned();
    let mut failures = world.resource_mut::<McpFailures>();
    let kind = if config_error || is_config_error_message(&reason) {
        FailureKind::ConfigHold
    } else {
        let prev_attempts = match failures.0.get(&id) {
            Some(prev)
                if prev.revision == launch.revision
                    && prev.credential_generation == launch.credential_generation =>
            {
                match prev.kind {
                    FailureKind::Transient { attempts, .. } => attempts,
                    FailureKind::ConfigHold => 0,
                }
            }
            _ => 0,
        };
        let attempts = prev_attempts.saturating_add(1);
        let delay = backoff_delay(attempts);
        let next_retry = Instant::now() + delay;
        let wake_timer = wake.map(|wake| {
            AbortOnDropHandle::new(runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                wake.wake();
            }))
        });
        FailureKind::Transient {
            attempts,
            next_retry,
            _wake_timer: wake_timer,
        }
    };
    failures.0.insert(
        id,
        FailureState {
            revision: launch.revision,
            credential_generation: launch.credential_generation,
            reason,
            kind,
        },
    );
}

pub(super) fn backoff_delay(attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(5);
    INITIAL_BACKOFF.saturating_mul(1u32 << exp).min(MAX_BACKOFF)
}

fn is_config_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("auth required")
        || lower.contains("authrequired")
        || lower.contains("insufficient scope")
        || lower.contains("insufficientscope")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("cannot find the file")
        || lower.contains("no such file")
        || lower.contains("not found on path")
        || lower.contains("neither a command nor an endpoint")
}

fn drain(world: &mut World) -> Vec<Event> {
    let mut events = world.resource_mut::<McpEvents>();
    let mut out = Vec::new();
    while let Ok(event) = events.receiver.try_recv() {
        out.push(event);
    }
    out
}

pub(super) async fn open(
    want: &Desired,
    registry: ToolServerHandle,
) -> Result<McpConnection, ConnectFailure> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    let connection = if !want.command.is_empty() {
        // A stdio server is a child process the host owns.
        let mut command = tokio::process::Command::new(&want.command);
        #[cfg(windows)]
        if want.command.eq_ignore_ascii_case("npx") || want.command.eq_ignore_ascii_case("npx.cmd")
        {
            command = rig_rmcp::rmcp::transport::which_command("npx.cmd").map_err(|error| {
                ConnectFailure {
                    reason: format!("npx.cmd was not found on PATH: {error}"),
                    config_error: true,
                }
            })?;
        }
        command.args(&want.args);
        if !want.cwd.is_empty() {
            command.current_dir(&want.cwd);
        }
        if !want.env.is_empty() {
            command.envs(&want.env);
        }
        if let Some(secret) = want.secret.as_deref().filter(|s| !s.is_empty())
            && !want.secret_env_var.is_empty()
        {
            command.env(&want.secret_env_var, secret);
        }
        let mut wrap = process_wrap::tokio::CommandWrap::from(command);
        // One process group, killed on drop, so a host that exits does not
        // orphan the server.
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrap.wrap(process_wrap::tokio::JobObject);
        wrap.wrap(process_wrap::tokio::KillOnDrop);
        let transport = TokioChildProcess::new(wrap).map_err(|error| ConnectFailure {
            config_error: error.kind() == std::io::ErrorKind::NotFound,
            reason: error.to_string(),
        })?;
        handler.connect(transport).await.map_err(|error| {
            let reason = error.to_string();
            ConnectFailure {
                config_error: is_config_error_message(&reason),
                reason,
            }
        })?
    } else if !want.endpoint.is_empty() {
        // The host's one HTTP client, so the server shares its connection pool
        // with every other endpoint the host calls.
        let client = crate::host::net::client().map_err(|reason| ConnectFailure {
            reason,
            config_error: true,
        })?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(want.endpoint.clone());
        for (name, value) in &want.headers {
            let header_name =
                HeaderName::from_bytes(name.trim().as_bytes()).map_err(|error| ConnectFailure {
                    reason: format!("invalid MCP header `{name}`: {error}"),
                    config_error: true,
                })?;
            let header_value = HeaderValue::from_str(value).map_err(|error| ConnectFailure {
                reason: format!("invalid MCP header value for `{name}`: {error}"),
                config_error: true,
            })?;
            config.custom_headers.insert(header_name, header_value);
        }
        if let Some(secret) = want
            .secret
            .as_deref()
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
        {
            let auth_header = want.auth_header.trim();
            if auth_header.is_empty() || auth_header.eq_ignore_ascii_case("authorization") {
                if let Some(bearer) = secret
                    .strip_prefix("Bearer ")
                    .or_else(|| secret.strip_prefix("bearer "))
                    .map(str::trim)
                {
                    config.auth_header = Some(bearer.to_owned());
                } else if secret.contains(char::is_whitespace) {
                    let header_value =
                        HeaderValue::from_str(secret).map_err(|error| ConnectFailure {
                            reason: format!("invalid MCP credential header: {error}"),
                            config_error: true,
                        })?;
                    config.custom_headers.insert(AUTHORIZATION, header_value);
                } else {
                    config.auth_header = Some(secret.to_owned());
                }
            } else {
                let header_name =
                    HeaderName::from_bytes(auth_header.as_bytes()).map_err(|error| {
                        ConnectFailure {
                            reason: format!("invalid MCP auth_header `{auth_header}`: {error}"),
                            config_error: true,
                        }
                    })?;
                let header_value =
                    HeaderValue::from_str(secret).map_err(|error| ConnectFailure {
                        reason: format!("invalid MCP credential value: {error}"),
                        config_error: true,
                    })?;
                config.custom_headers.insert(header_name, header_value);
            }
        }
        let transport = StreamableHttpClientTransport::with_client(client, config);
        handler.connect(transport).await.map_err(|error| {
            let reason = error.to_string();
            ConnectFailure {
                config_error: is_config_error_message(&reason),
                reason,
            }
        })?
    } else {
        return Err(ConnectFailure {
            reason: "the MCP connection names neither a command nor an endpoint".to_owned(),
            config_error: true,
        });
    };
    Ok(connection)
}

/// Clears any backoff or configuration hold on `id` so the supervisor attempts
/// a fresh connection on the next frame (used by `CheckConnection`).
pub(crate) fn request_reconnect(world: &mut World, id: &str) {
    if let Some(mut failures) = world.get_resource_mut::<McpFailures>() {
        failures.0.remove(id);
    }
}

/// Inspects the current MCP supervisor state for a specific connection revision
/// and credential generation.
pub(crate) fn check_state(world: &World, id: &str, revision: u64) -> CheckState {
    let cred_gen = world
        .get_resource::<Credentials>()
        .map(Credentials::generation);
    if let Some(server) = world
        .get_resource::<McpServers>()
        .and_then(|servers| servers.0.get(id))
        && server.launch.revision == revision
        && server.launch.credential_generation == cred_gen
        && !server.connection.is_closed()
    {
        let catalog = server.registry.snapshot();
        let mut names: Vec<String> = catalog
            .definitions()
            .iter()
            .map(|def| def.name.as_str().to_owned())
            .collect();
        names.sort();
        names.dedup();
        return CheckState::Connected(names);
    }
    if world
        .get_resource::<Connecting>()
        .is_some_and(|connecting| {
            connecting
                .0
                .get(id)
                .is_some_and(|pending| pending.launch.revision == revision)
        })
    {
        return CheckState::Connecting;
    }
    if let Some(failure) = world
        .get_resource::<McpFailures>()
        .and_then(|failures| failures.0.get(id))
        && failure.revision == revision
        && failure.credential_generation == cred_gen
    {
        return CheckState::Failed(failure.reason.clone());
    }
    CheckState::Connecting
}
