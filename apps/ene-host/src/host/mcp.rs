//! MCP servers as ordinary tools (tech-stack §B).
//!
//! One enabled `Mcp` connection is one server. The host connects on its
//! runtime, lists the server's tools and registers each allowed one like any
//! other tool, so the agent does not care whether a tool is built in, served by
//! a client or served by an MCP process.
//!
//! The tools belong to the operator's own configuration rather than to a
//! client's declaration, so they run without an approval card: the gate passes
//! them the same way it passes the built-in host tools, and stays in charge of
//! what a *client* may ask the host to do.
//!
//! A server that is disabled, deleted, re-configured or disconnected loses its
//! tools: the connection closes and the tool entities despawn.
use crate::host::{
    db::Store,
    resources::HostRuntime,
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_agent::tool::{
    ToolCatalog,
    server::{ToolServer, ToolServerHandle},
};
use rig_core::{
    effect::{Key, family},
    message::ToolName,
};
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
    collections::{HashMap, HashSet},
    path::Path,
};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

/// How many connect results may queue before a producer waits. A connect
/// happens once per server, so this is a bound, not a throughput target.
const QUEUE: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Launch {
    revision: u64,
    owner: String,
}

/// The registration an MCP tool entity came from: the library generation that
/// keeps a stale refresh from replacing a newer handler, and the server that
/// owns it. The entity carries this, so no side map has to stay in step.
#[derive(Component)]
struct McpTool {
    server: String,
    key: Key<family::Tool>,
}

/// One connected server and the tool entities it owns.
struct Server {
    connection: McpConnection,
    launch: Launch,
    registry: ToolServerHandle,
    label: String,
    allowed: Vec<String>,
}

/// The servers that are up right now, keyed by connection id.
#[derive(Resource, Default)]
pub(crate) struct McpServers(HashMap<String, Server>);

/// Connection ids a connect task is running for.
#[derive(Resource, Default)]
struct Connecting(HashMap<String, Pending>);

struct Pending {
    launch: Launch,
    _task: AbortOnDropHandle<()>,
}

enum Event {
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
    },
}

#[derive(Resource)]
struct McpEvents {
    sender: mpsc::Sender<Event>,
    receiver: mpsc::Receiver<Event>,
}

impl Default for McpEvents {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self { sender, receiver }
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<McpServers>()
        .init_resource::<Connecting>()
        .init_resource::<McpEvents>()
        .init_resource::<McpSettings>()
        .add_systems(Update, sync.after(AgentLoop));
}

/// What one settings row asks the host to connect to.
#[derive(Clone)]
struct Desired {
    revision: u64,
    name: String,
    command: String,
    args: Vec<String>,
    endpoint: String,
    allowed: Vec<String>,
}

#[derive(Resource, Default)]
struct McpSettings {
    revision: Option<u64>,
    desired: Vec<(String, Desired)>,
}

fn desired(store: &Store) -> Vec<(String, Desired)> {
    let Ok(settings) = store.settings() else {
        return Vec::new();
    };
    settings
        .connections
        .iter()
        .filter(|connection| connection.enabled && connection.kind == ConnectionKind::Mcp)
        .filter_map(|connection| {
            let ConnectionConfig::Mcp(config) = &connection.config else {
                return None;
            };
            Some((
                connection.id.clone(),
                Desired {
                    revision: connection.revision,
                    name: connection.name.clone(),
                    command: config.command.trim().to_owned(),
                    args: config.args.clone(),
                    endpoint: config.endpoint.endpoint.trim().to_owned(),
                    allowed: config.allowed_tools.clone(),
                },
            ))
        })
        .collect()
}

/// Connects what the settings ask for, retires what they no longer do, and
/// attaches the tools a handler listed.
fn sync(world: &mut World) {
    let closing = world
        .get_resource::<crate::host::resources::HostState>()
        .is_some_and(|state| state.closing || state.failed.is_some());
    if closing {
        world.resource_mut::<Connecting>().0.clear();
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
    let Ok(revision) = store.settings_revision() else {
        return;
    };
    if world.resource::<McpSettings>().revision != Some(revision) {
        let desired = desired(&store);
        *world.resource_mut::<McpSettings>() = McpSettings {
            revision: Some(revision),
            desired,
        };
    }
    let wanted = world.resource::<McpSettings>().desired.clone();

    world.resource_mut::<Connecting>().0.retain(|id, pending| {
        wanted
            .iter()
            .any(|(wanted, want)| wanted == id && want.revision == pending.launch.revision)
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
                    .is_some_and(|(_, want)| want.revision != server.launch.revision)
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in retire {
        retire_one(world, &id);
    }

    project_servers(world);

    let sender = world.resource::<McpEvents>().sender.clone();
    for (id, want) in wanted {
        if world.resource::<McpServers>().0.contains_key(&id)
            || world.resource::<Connecting>().0.contains_key(&id)
        {
            continue;
        }
        let registry = ToolServer::new().run();
        let launch = Launch {
            revision: want.revision,
            owner: registry.owner(),
        };
        let event_id = id.clone();
        let event_launch = launch.clone();
        let sender = sender.clone();
        let task = runtime.spawn(async move {
            let event = match open(&want, registry.clone()).await {
                Ok(connection) => Event::Connected {
                    id: event_id,
                    launch: event_launch,
                    connection,
                    registry,
                },
                Err(reason) => Event::Failed {
                    id: event_id,
                    launch: event_launch,
                    reason,
                },
            };
            if let Err(error) = sender.send(event).await
                && let Event::Connected { connection, .. } = error.0
            {
                connection.close().await;
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

fn accept_event(
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
        && wanted
            .iter()
            .any(|(wanted, want)| wanted == id && want.revision == launch.revision);
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
            let want = &wanted.iter().find(|(wanted, _)| wanted == &id).unwrap().1;
            attach(world, &id, launch, connection, registry, want);
        }
        Event::Failed { id, reason, .. } => {
            tracing::warn!(connection = %id, "the MCP server did not connect: {reason}");
        }
    }
}

fn drain(world: &mut World) -> Vec<Event> {
    let mut events = world.resource_mut::<McpEvents>();
    let mut out = Vec::new();
    while let Ok(event) = events.receiver.try_recv() {
        out.push(event);
    }
    out
}

async fn open(want: &Desired, registry: ToolServerHandle) -> Result<McpConnection, String> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    let connection = if !want.command.is_empty() {
        // A stdio server is a child process the host owns.
        let args = want.args.clone();
        let mut wrap =
            process_wrap::tokio::CommandWrap::with_new(Path::new(&want.command), |cmd| {
                cmd.args(&args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped());
            });
        // One process group, killed on drop, so a host that exits does not
        // orphan the server.
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrap.wrap(process_wrap::tokio::JobObject);
        wrap.wrap(process_wrap::tokio::KillOnDrop);
        let transport = TokioChildProcess::new(wrap).map_err(|error| error.to_string())?;
        handler
            .connect(transport)
            .await
            .map_err(|error| error.to_string())?
    } else if !want.endpoint.is_empty() {
        // The host's one HTTP client, so the server shares its connection pool
        // with every other endpoint the host calls.
        let client = crate::host::net::client()?;
        let transport = StreamableHttpClientTransport::with_client(
            client,
            StreamableHttpClientTransportConfig::with_uri(want.endpoint.clone()),
        );
        handler
            .connect(transport)
            .await
            .map_err(|error| error.to_string())?
    } else {
        return Err("the MCP connection names neither a command nor an endpoint".to_owned());
    };
    Ok(connection)
}

/// The managed registry owns generations and liveness. ECS owns exposed names.
fn attach(
    world: &mut World,
    id: &str,
    launch: Launch,
    connection: McpConnection,
    registry: ToolServerHandle,
    want: &Desired,
) {
    retire_one(world, id);
    world.resource_mut::<McpServers>().0.insert(
        id.to_owned(),
        Server {
            connection,
            launch,
            registry,
            label: want.name.clone(),
            allowed: want.allowed.clone(),
        },
    );
}

fn project_servers(world: &mut World) {
    let ids: Vec<String> = world.resource::<McpServers>().0.keys().cloned().collect();
    let mut changed = false;
    for id in ids {
        let Some(server) = world.resource_mut::<McpServers>().0.remove(&id) else {
            continue;
        };
        let catalog = server.registry.snapshot();
        changed |= project(world, &id, &server, Some(&catalog));
        world.resource_mut::<McpServers>().0.insert(id, server);
    }
    if changed {
        refresh_access(world);
    }
}

fn project(world: &mut World, id: &str, server: &Server, catalog: Option<&ToolCatalog>) -> bool {
    let mut desired = HashMap::new();
    for definition in catalog.into_iter().flat_map(ToolCatalog::definitions) {
        let raw = definition.name.as_str();
        if !server.allowed.is_empty() && !server.allowed.iter().any(|name| name == raw) {
            continue;
        }
        let Some(exposed) = exposed_name(&server.label, id, raw) else {
            continue;
        };
        let Ok(name) = ToolName::new(exposed.clone()) else {
            continue;
        };
        let registration = catalog.unwrap().get(raw).unwrap();
        let mut definition = (**definition).clone();
        definition.name = name;
        desired.insert(exposed, (definition, registration));
    }
    let mut current: HashMap<String, (Entity, Key<family::Tool>)> = {
        let mut query = world.query::<(Entity, &ToolDef, &McpTool)>();
        query
            .iter(world)
            .filter(|(_, _, tool)| tool.server == id)
            .map(|(entity, definition, tool)| {
                (
                    definition.0.name.as_str().to_owned(),
                    (entity, tool.key.clone()),
                )
            })
            .collect()
    };
    let mut changed = false;
    for (name, (definition, tool)) in desired {
        let existing = current.remove(&name);
        if existing.as_ref().is_some_and(|(_, key)| key == tool.key()) {
            continue;
        }
        if let Some((entity, _)) = existing {
            if let Ok(entity) = world.get_entity_mut(entity) {
                entity.despawn();
            }
            changed = true;
        }
        if let Some(entity) =
            world.spawn_tool(definition, tool.handler().clone(), ToolOptions::default())
        {
            world.entity_mut(entity).insert(McpTool {
                server: id.to_owned(),
                key: tool.key().clone(),
            });
            changed = true;
        }
    }
    for (_, (entity, _)) in current {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
            changed = true;
        }
    }
    changed
}

/// The model-facing names MCP servers contribute right now. The tool entities
/// are the only owner: the gate and the per-agent access read this set instead
/// of a second table kept in step with them.
pub(crate) fn tool_names(world: &mut World) -> HashSet<String> {
    let mut query = world.query::<(&ToolDef, &McpTool)>();
    query
        .iter(world)
        .map(|(definition, _)| definition.0.name.as_str().to_owned())
        .collect()
}

/// Raw tool names currently registered for a specific connected MCP server id.
pub(crate) fn server_tool_names(world: &World, id: &str, revision: u64) -> Option<Vec<String>> {
    let server = world.resource::<McpServers>().0.get(id)?;
    if server.launch.revision != revision || server.connection.is_closed() {
        return None;
    }
    let catalog = server.registry.snapshot();
    let mut names: Vec<String> = catalog
        .definitions()
        .iter()
        .map(|def| def.name.as_str().to_owned())
        .collect();
    names.sort();
    names.dedup();
    Some(names)
}

fn refresh_access(world: &mut World) {
    if let Err(error) = crate::host::client::dynamic::sync_tools(world) {
        tracing::warn!(%error, "the MCP tools are not offered");
    }
}

/// Closes one server and removes the tools it contributed.
fn retire_one(world: &mut World, id: &str) {
    let Some(server) = world.resource_mut::<McpServers>().0.remove(id) else {
        return;
    };
    if project(world, id, &server, None) {
        refresh_access(world);
    }
    // Closing waits for the transport, so it runs off the host thread.
    if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
        let connection = server.connection;
        runtime.spawn(async move { connection.close().await });
    }
}

/// The model-facing name for one server's tool: `mcp_<server>_<tool>`. The
/// server part keeps two servers that offer the same tool name apart. A part
/// the provider grammar cannot carry is refused rather than rewritten onto
/// another tool.
fn exposed_name(label: &str, id: &str, tool: &str) -> Option<String> {
    let mut name = String::from("mcp_");
    if !push_allowed(&mut name, label) {
        let short: String = id.chars().take(8).collect();
        if !push_allowed(&mut name, &short) {
            return None;
        }
    }
    name.push('_');
    if !push_allowed(&mut name, tool) || name.len() > 64 {
        return None;
    }
    Some(name)
}

fn push_allowed(out: &mut String, text: &str) -> bool {
    if text.is_empty()
        || !text.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return false;
    }
    out.push_str(text);
    true
}

/// The plugin the host adds once.
#[derive(Default)]
pub(crate) struct McpPlugin;

impl Plugin for McpPlugin {
    fn build(&self, app: &mut App) {
        register(app);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::tool::{DynamicTool, ManagedToolSink, ToolContext, ToolOutput};

    fn want(name: &str) -> Desired {
        Desired {
            revision: 1,
            name: name.into(),
            command: String::new(),
            args: Vec::new(),
            endpoint: String::new(),
            allowed: Vec::new(),
        }
    }

    /// A server that serves one text tool over an in-process duplex pair, so
    /// the whole client path runs without a child process or a network.
    #[derive(Clone)]
    struct Echo {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        listed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        generation: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Echo {
        fn new() -> Self {
            Self {
                calls: Default::default(),
                listed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                generation: Default::default(),
            }
        }
    }

    async fn connect_echo(
        echo: Echo,
    ) -> (
        McpConnection,
        ToolServerHandle,
        rig_rmcp::rmcp::service::Peer<rig_rmcp::rmcp::RoleServer>,
        tokio::task::JoinHandle<()>,
    ) {
        use rig_rmcp::rmcp::{
            RoleClient, RoleServer, service::serve_directly, transport::async_rw::AsyncRwTransport,
        };
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let (peer_sender, peer_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let transport = AsyncRwTransport::<RoleServer, _, _>::new(server_read, server_write);
            let running = serve_directly(echo, transport, None);
            let _ = peer_sender.send(running.peer().clone());
            let _ = running.waiting().await;
        });
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(client_read, client_write);
        let registry = ToolServer::new().run();
        let connection = McpClientHandler::new(ClientConfig::default(), registry.clone())
            .connect(transport)
            .await
            .expect("the server lists its tools");
        (connection, registry, peer_receiver.await.unwrap(), server)
    }

    impl rig_rmcp::rmcp::ServerHandler for Echo {
        fn get_info(&self) -> rig_rmcp::rmcp::model::ServerConfig {
            rig_rmcp::rmcp::model::ServerConfig::default()
        }

        async fn list_tools(
            &self,
            _request: Option<rig_rmcp::rmcp::model::PaginatedRequestParams>,
            _context: rig_rmcp::rmcp::service::RequestContext<rig_rmcp::rmcp::RoleServer>,
        ) -> Result<rig_rmcp::rmcp::model::ListToolsResult, rig_rmcp::rmcp::ErrorData> {
            Ok(rig_rmcp::rmcp::model::ListToolsResult::with_all_items(
                if self.listed.load(std::sync::atomic::Ordering::SeqCst) {
                    vec![rig_rmcp::rmcp::model::Tool::new(
                        "echo",
                        format!("Echo generation {}", self.generation.load(std::sync::atomic::Ordering::SeqCst)),
                        serde_json::json!({"type":"object","properties":{"value":{"type":"integer","minimum":self.generation.load(std::sync::atomic::Ordering::SeqCst)}}}).as_object().unwrap().clone(),
                    )]
                } else {
                    Vec::new()
                },
            ))
        }

        async fn call_tool(
            &self,
            request: rig_rmcp::rmcp::model::CallToolRequestParams,
            _context: rig_rmcp::rmcp::service::RequestContext<rig_rmcp::rmcp::RoleServer>,
        ) -> Result<rig_rmcp::rmcp::model::CallToolResponse, rig_rmcp::rmcp::ErrorData> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(rig_rmcp::rmcp::model::CallToolResponse::Complete(
                rig_rmcp::rmcp::model::CallToolResult::success(vec![
                    rig_rmcp::rmcp::model::ContentBlock::text(format!("called {}", request.name)),
                ]),
            ))
        }
    }

    #[tokio::test]
    async fn http_tools_use_the_shared_redirect_refusing_client() {
        use axum::{Json, Router, response::IntoResponse, routing::post};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = hits.clone();
        let app = Router::new()
            .route("/mcp", post(move |Json(request): Json<serde_json::Value>| {
                requests.fetch_add(1, Ordering::SeqCst);
                async move {
                    let Some(id) = request.get("id") else {
                        return http::StatusCode::ACCEPTED.into_response();
                    };
                    let result = match request["method"].as_str().unwrap() {
                        "initialize" => serde_json::json!({
                            "protocolVersion":request["params"]["protocolVersion"],
                            "capabilities":{"tools":{}},
                            "serverInfo":{"name":"loopback","version":"1"}
                        }),
                        "tools/list" => serde_json::json!({"tools":[{"name":"echo","description":"HTTP echo","inputSchema":{"type":"object"}}]}),
                        "tools/call" => serde_json::json!({"content":[{"type":"text","text":"called http"}],"isError":false}),
                        method => panic!("unexpected HTTP MCP request {method}"),
                    };
                    Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
                }
            }))
            .route("/redirect", post(|| async { (http::StatusCode::TEMPORARY_REDIRECT, [(http::header::LOCATION, "/mcp")]) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut desired = want("http");
        desired.endpoint = format!("http://{address}/mcp");
        let registry = ToolServer::new().run();
        let connection = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            open(&desired, registry.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let result = registry
            .snapshot()
            .get("echo")
            .unwrap()
            .execute("{}".into(), &mut ToolContext::new())
            .await;
        assert_eq!(result.output().as_text(), Some("called http"));
        connection.close().await;
        assert!(connection.is_closed());
        let before = hits.load(Ordering::SeqCst);
        desired.endpoint = format!("http://{address}/redirect");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                open(&desired, ToolServer::new().run())
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before,
            "the checked endpoint cannot redirect MCP elsewhere"
        );
        server.abort();
    }

    #[cfg(target_os = "linux")]
    struct PendingPeer {
        marker: std::path::PathBuf,
        process: Option<(rustix::process::Pid, rustix::fd::OwnedFd)>,
    }

    #[cfg(target_os = "linux")]
    impl PendingPeer {
        fn new(root: &Path, name: &str) -> Self {
            Self {
                marker: root.join(name),
                process: None,
            }
        }

        fn config(&self) -> crate::host::settings::McpConnection {
            crate::host::settings::McpConnection {
                endpoint: crate::host::settings::Endpoint {
                    adapter: "mcp".into(),
                    options: "{}".into(),
                    ..Default::default()
                },
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '%s' \"$$\" > \"$1\"; exec sleep 300".into(),
                    "owned-mcp-peer".into(),
                    self.marker.to_str().unwrap().into(),
                ],
                allowed_tools: Vec::new(),
            }
        }

        async fn started(&mut self) {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(pid) = std::fs::read_to_string(&self.marker)
                        .ok()
                        .and_then(|text| text.parse::<i32>().ok())
                        .and_then(rustix::process::Pid::from_raw)
                    {
                        let fd =
                            rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                                .unwrap();
                        self.process = Some((pid, fd));
                        assert!(rustix::process::test_kill_process(pid).is_ok());
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the owned never-initializing peer records its PID");
        }

        async fn stopped(&mut self, action: &str) {
            let pid = self.process.as_ref().unwrap().0;
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("owned pending MCP PID {pid} survived {action}"));
            self.process = None;
            eprintln!("owned pending MCP PID {pid} disappeared after {action}");
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for PendingPeer {
        fn drop(&mut self) {
            if let Some((_, fd)) = &self.process {
                let _ = rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pending_launch_invalidation_reaps_never_initialized_children() {
        use crate::host::{
            resources::HostState,
            settings::{Connection, CredentialState},
        };

        for action in ["disable", "revision", "shutdown", "failure", "world drop"] {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
            let mut peer = PendingPeer::new(root.path(), "initial.pid");
            let mut row = Connection {
                id: "pending-mcp".into(),
                name: "pending".into(),
                enabled: true,
                kind: ConnectionKind::Mcp,
                config: ConnectionConfig::Mcp(peer.config()),
                credential_state: CredentialState::Absent,
                revision: 0,
                last_check: None,
            };
            store.update_settings(1, &[(row.clone(), 0)]).unwrap();
            let mut world = World::new();
            world.insert_resource(store.clone());
            world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
            world.init_resource::<HostState>();
            world.init_resource::<McpServers>();
            world.init_resource::<Connecting>();
            world.init_resource::<McpSettings>();
            world.init_resource::<McpEvents>();
            sync(&mut world);
            peer.started().await;
            assert!(world.resource::<McpServers>().0.is_empty());
            let old_launch = world.resource::<Connecting>().0[&row.id].launch.clone();
            let mut replacement = PendingPeer::new(root.path(), "replacement.pid");
            match action {
                "disable" => {
                    row.enabled = false;
                    store.update_settings(2, &[(row.clone(), 1)]).unwrap();
                }
                "revision" => {
                    row.config = ConnectionConfig::Mcp(replacement.config());
                    store.update_settings(2, &[(row.clone(), 1)]).unwrap();
                }
                "shutdown" => world.resource_mut::<HostState>().closing = true,
                "failure" => world.resource_mut::<HostState>().failed = Some("test failure".into()),
                "world drop" => {
                    drop(world);
                    peer.stopped(action).await;
                    continue;
                }
                _ => unreachable!(),
            }
            sync(&mut world);
            if action == "revision" {
                replacement.started().await;
            }
            peer.stopped(action).await;
            assert!(world.resource::<McpServers>().0.is_empty());
            if action == "revision" {
                let new_launch = world.resource::<Connecting>().0[&row.id].launch.clone();
                assert_ne!(old_launch, new_launch);
                accept_event(
                    &mut world,
                    &[(
                        row.id.clone(),
                        Desired {
                            revision: 2,
                            ..want("pending")
                        },
                    )],
                    Event::Failed {
                        id: row.id.clone(),
                        launch: old_launch,
                        reason: "late failure".into(),
                    },
                    &tokio::runtime::Handle::current(),
                );
                assert_eq!(world.resource::<Connecting>().0[&row.id].launch, new_launch);
                assert!(
                    rustix::process::test_kill_process(replacement.process.as_ref().unwrap().0)
                        .is_ok()
                );
                world.resource_mut::<HostState>().closing = true;
                sync(&mut world);
                replacement.stopped("replacement shutdown").await;
            }
            assert!(world.resource::<Connecting>().0.is_empty());
        }
    }

    fn local_tool(name: &str, generation: usize) -> DynamicTool {
        DynamicTool::new(
            ToolName::new(name).unwrap(),
            format!("generation {generation}"),
            serde_json::json!({"type":"object","properties":{"version":{"const":generation}}}),
            move |_| Box::pin(async move { Ok(ToolOutput::text(generation.to_string())) }),
        )
    }

    fn tool_entity(world: &mut World, name: &str) -> Option<Entity> {
        let mut query = world.query::<(Entity, &ToolDef)>();
        query
            .iter(world)
            .find(|(_, definition)| definition.0.name.as_str() == name)
            .map(|(entity, _)| entity)
    }

    fn server_tools(world: &mut World, id: &str) -> usize {
        let mut query = world.query::<&McpTool>();
        query.iter(world).filter(|tool| tool.server == id).count()
    }

    #[tokio::test]
    async fn registry_refresh_replaces_handlers_and_keeps_pinned_calls() {
        let (connection, registry, _, task) = connect_echo(Echo::new()).await;
        let expected = registry.add_managed_tools(vec![local_tool("echo", 1)]);
        let pinned = registry.snapshot();
        let server = Server {
            connection: connection.clone(),
            launch: Launch {
                revision: 1,
                owner: registry.owner(),
            },
            registry: registry.clone(),
            label: "notes".into(),
            allowed: Vec::new(),
        };
        let mut world = World::new();
        assert!(project(&mut world, "c1", &server, Some(&pinned)));
        let old_entity = tool_entity(&mut world, "mcp_notes_echo").unwrap();
        let refreshed =
            registry.reconcile_managed_tools(expected.clone(), vec![local_tool("echo", 2)]);
        let current = registry.snapshot();
        assert!(project(&mut world, "c1", &server, Some(&current)));
        let entity = tool_entity(&mut world, "mcp_notes_echo").unwrap();
        assert_ne!(old_entity, entity);
        assert!(world.get_entity(old_entity).is_err());
        assert_eq!(
            world.get::<ToolDef>(entity).unwrap().0.parameters["properties"]["version"]["const"],
            2
        );
        assert_eq!(
            pinned
                .get("echo")
                .unwrap()
                .execute("{}".into(), &mut ToolContext::new())
                .await
                .output()
                .as_text(),
            Some("1")
        );
        assert_eq!(
            current
                .get("echo")
                .unwrap()
                .execute("{}".into(), &mut ToolContext::new())
                .await
                .output()
                .as_text(),
            Some("2")
        );
        registry.reconcile_managed_tools(expected, vec![local_tool("echo", 3)]);
        assert_eq!(registry.snapshot().key("echo"), current.key("echo"));
        registry.reconcile_managed_tools(refreshed, Vec::new());
        assert!(project(
            &mut world,
            "c1",
            &server,
            Some(&registry.snapshot())
        ));
        assert!(tool_entity(&mut world, "mcp_notes_echo").is_none());
        assert!(world.get_entity(entity).is_err());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe = alive.clone();
        registry.add_managed_tools(vec![
            local_tool("echo", 4)
                .with_liveness(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
        ]);
        assert!(project(
            &mut world,
            "c1",
            &server,
            Some(&registry.snapshot())
        ));
        alive.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(project(
            &mut world,
            "c1",
            &server,
            Some(&registry.snapshot())
        ));
        assert!(tool_entity(&mut world, "mcp_notes_echo").is_none());
        connection.close().await;
        task.abort();
    }

    #[tokio::test]
    async fn projection_refuses_collisions_and_retires_only_owned_entities() {
        let (first, registry, _, task1) = connect_echo(Echo::new()).await;
        let (second, other_registry, _, task2) = connect_echo(Echo::new()).await;
        registry.add_managed_tools(vec![local_tool("other", 1), local_tool("bad.name", 1)]);
        let mut harness = crate::host::tests::Harness::new([]);
        let world = harness.app.world_mut();
        world.init_resource::<McpServers>();
        world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        let (definition, handler, _) = local_tool("mcp_notes_echo", 0).into_parts();
        let foreign = world
            .spawn_tool(definition, handler, ToolOptions::default())
            .unwrap();
        let mut desired = want("notes");
        desired.allowed = vec!["echo".into(), "bad.name".into()];
        attach(
            world,
            "c1",
            Launch {
                revision: 1,
                owner: registry.owner(),
            },
            first.clone(),
            registry,
            &desired,
        );
        project_servers(world);
        assert_eq!(server_tools(world, "c1"), 0);
        assert!(!tool_names(world).contains("mcp_notes_echo"));
        assert!(world.get_entity(foreign).is_ok());
        world.entity_mut(foreign).despawn();
        project_servers(world);
        let entity = tool_entity(world, "mcp_notes_echo").unwrap();
        assert!(!tool_names(world).contains("mcp_notes_other"));
        assert_eq!(server_tools(world, "c1"), 1);
        attach(
            world,
            "c2",
            Launch {
                revision: 1,
                owner: other_registry.owner(),
            },
            second.clone(),
            other_registry,
            &want("notes"),
        );
        project_servers(world);
        assert_eq!(server_tools(world, "c2"), 0);
        retire_one(world, "c2");
        assert!(world.get_entity(entity).is_ok());
        assert!(tool_names(world).contains("mcp_notes_echo"));
        retire_one(world, "c1");
        assert!(world.get_entity(entity).is_err());
        assert!(tool_names(world).is_empty());
        first.closed().await;
        second.closed().await;
        task1.abort();
        task2.abort();
    }

    #[tokio::test]
    async fn stale_launch_results_and_notifications_cannot_change_the_new_owner() {
        let old_echo = Echo::new();
        let old_generation = old_echo.generation.clone();
        let (old, old_registry, old_peer, task1) = connect_echo(old_echo).await;
        let (new, new_registry, _, task2) = connect_echo(Echo::new()).await;
        let old_launch = Launch {
            revision: 1,
            owner: old_registry.owner(),
        };
        let new_launch = Launch {
            revision: 1,
            owner: new_registry.owner(),
        };
        assert_ne!(old_launch, new_launch);
        let mut harness = crate::host::tests::Harness::new([]);
        let world = harness.app.world_mut();
        world.init_resource::<McpServers>();
        world.init_resource::<Connecting>();
        let runtime = tokio::runtime::Handle::current();
        world.insert_resource(HostRuntime(runtime.clone()));
        world.resource_mut::<Connecting>().0.insert(
            "c1".into(),
            Pending {
                launch: new_launch.clone(),
                _task: AbortOnDropHandle::new(runtime.spawn(std::future::pending())),
            },
        );
        let wanted = vec![("c1".into(), want("notes"))];
        accept_event(
            world,
            &wanted,
            Event::Failed {
                id: "c1".into(),
                launch: old_launch.clone(),
                reason: "old failure".into(),
            },
            &runtime,
        );
        assert_eq!(
            world
                .resource::<Connecting>()
                .0
                .get("c1")
                .map(|pending| &pending.launch),
            Some(&new_launch)
        );
        let mut changed_revision = wanted.clone();
        changed_revision[0].1.revision = 2;
        accept_event(
            world,
            &changed_revision,
            Event::Failed {
                id: "c1".into(),
                launch: new_launch.clone(),
                reason: "obsolete revision".into(),
            },
            &runtime,
        );
        assert_eq!(
            world
                .resource::<Connecting>()
                .0
                .get("c1")
                .map(|pending| &pending.launch),
            Some(&new_launch)
        );
        accept_event(
            world,
            &wanted,
            Event::Connected {
                id: "c1".into(),
                launch: new_launch,
                connection: new.clone(),
                registry: new_registry,
            },
            &runtime,
        );
        assert!(world.resource::<Connecting>().0.is_empty());
        project_servers(world);
        let current = tool_entity(world, "mcp_notes_echo").unwrap();
        old_generation.store(99, std::sync::atomic::Ordering::SeqCst);
        old_peer.notify_tool_list_changed().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if old_registry.snapshot().definitions()[0].description == "Echo generation 99" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the old launch processed its late notification");
        accept_event(
            world,
            &wanted,
            Event::Connected {
                id: "c1".into(),
                launch: old_launch,
                connection: old.clone(),
                registry: old_registry.clone(),
            },
            &runtime,
        );
        old_registry.add_managed_tools(vec![local_tool("late", 99)]);
        project_servers(world);
        assert_eq!(tool_entity(world, "mcp_notes_echo"), Some(current));
        assert!(!tool_names(world).contains("mcp_notes_late"));
        retire_one(world, "c1");
        old_registry.add_managed_tools(vec![local_tool("after_retire", 99)]);
        project_servers(world);
        assert!(world.get_entity(current).is_err());
        assert!(tool_names(world).is_empty());
        old.closed().await;
        new.closed().await;
        task1.abort();
        task2.abort();
    }

    /// The whole path: a listed MCP tool is attached to the real host app,
    /// offered exclusively to the work agent ("hand", never the head), passes
    /// the gate and runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_mcp_tool_runs_through_the_gate() {
        use crate::host::agent::{CompanionAgent, WorkAgent};
        use crate::host::tests::Harness;
        use rig_core::completion::Message;
        use rig_core::message::{ToolResultContent, UserContent};
        use rig_core::test_utils::MockStreamEvent;
        let echo = Echo::new();
        let calls = echo.calls.clone();
        let listed = echo.listed.clone();
        let generation = echo.generation.clone();
        let (connection, registry, peer, server) = connect_echo(echo).await;

        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call("call-1", "mcp_notes_echo", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("呼んだよ"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("完了を確認したよ"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let world = harness.app.world_mut();
        world.init_resource::<McpServers>();
        attach(
            world,
            "c1",
            Launch {
                revision: 1,
                owner: registry.owner(),
            },
            connection,
            registry,
            &want("notes"),
        );
        project_servers(world);
        assert!(tool_names(world).contains("mcp_notes_echo"));

        let head_allows_mcp = world
            .query_filtered::<&ToolAccess, With<CompanionAgent>>()
            .iter(world)
            .any(|access| access.allows("mcp_notes_echo"));
        assert!(
            !head_allows_mcp,
            "companion head must not receive general MCP tools"
        );

        let companion_id = harness.companion.clone();
        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id,
                instruction: "ノート読んで".into(),
                ..Default::default()
            },
        )
        .expect("work session starts");

        harness.run_until(|world| {
            let hand_allows_mcp = world
                .query_filtered::<&ToolAccess, With<WorkAgent>>()
                .iter(world)
                .any(|access| access.allows("mcp_notes_echo"));
            if !hand_allows_mcp {
                return false;
            }
            world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conversation| {
                    conversation.entries().any(|entry| match entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == "mcp_notes_echo"
                                && !result.is_error
                                && result.content.iter().any(|block|
                                    matches!(block, ToolResultContent::Text(text)
                                        if text.text == "called echo")))
                        }),
                        _ => false,
                    })
                })
        });
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the model's call reached the MCP server"
        );
        let old_entity = tool_entity(harness.app.world_mut(), "mcp_notes_echo").unwrap();
        generation.store(2, std::sync::atomic::Ordering::SeqCst);
        peer.notify_tool_list_changed().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let world = harness.app.world_mut();
                project_servers(world);
                let current = tool_entity(world, "mcp_notes_echo").unwrap();
                if current != old_entity {
                    let definition = &world.get::<ToolDef>(current).unwrap().0;
                    assert_eq!(definition.description, "Echo generation 2");
                    assert_eq!(definition.parameters["properties"]["value"]["minimum"], 2);
                    assert!(world.get_entity(old_entity).is_err());
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a same-name refresh replaces the schema and entity");
        listed.store(false, std::sync::atomic::Ordering::SeqCst);
        peer.notify_tool_list_changed().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let world = harness.app.world_mut();
                project_servers(world);
                if !tool_names(world).contains("mcp_notes_echo") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("list_changed removes the tool from the host");
        server.abort();
    }
}
