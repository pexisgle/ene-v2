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
//!
//! The registry is the handler's own sink: [`rig_rmcp::McpClientHandler`]
//! refreshes its tool list into [`EcsTools`], which renames each tool to its
//! model-facing name and pushes the result to the host thread. The host thread
//! is the only writer of tool entities, so a refresh that arrives while the
//! frame is running waits for the next one instead of racing it.
use crate::host::{
    db::Store,
    resources::{HostRuntime, HostToolNames},
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::ToolDefinition,
    message::ToolName,
    serve::ErasedHandler,
    tool::{DynamicTool, ManagedToolSink, ManagedToolToken},
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
                StreamableHttpClientWorker,
            },
        },
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

/// How many connect results may queue before a producer waits. A connect
/// happens once per server, so this is a bound, not a throughput target.
const QUEUE: usize = 32;

/// One connected server and the tool entities it owns.
struct Server {
    connection: McpConnection,
    /// The connection revision these tools were built from.
    revision: u64,
    /// The host tool entity per model-facing name.
    tools: HashMap<String, Entity>,
}

/// The servers that are up right now, keyed by connection id.
#[derive(Resource, Default)]
pub(crate) struct McpServers(HashMap<String, Server>);

/// Connection ids a connect task is running for.
#[derive(Resource, Default)]
struct Connecting(HashSet<String>);

enum Event {
    Connected {
        id: String,
        revision: u64,
        connection: McpConnection,
    },
    Failed {
        id: String,
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

/// One server's tool list as its handler last reported it.
struct Update {
    id: String,
    /// Every tool it serves now, by model-facing name.
    tools: Vec<(String, ToolDefinition, ErasedHandler)>,
}

/// The updates the handlers pushed, drained by the host thread. An update for
/// a server the frame has not attached yet waits here for the next one.
#[derive(Resource, Default, Clone)]
struct McpUpdates(Arc<Mutex<Vec<Update>>>);

impl McpUpdates {
    fn push(&self, update: Update) {
        self.0.lock().unwrap().push(update);
    }

    fn take(&self) -> Vec<Update> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// Puts back what this frame could not apply, ahead of anything newer.
    fn restore(&self, deferred: Vec<Update>) {
        let mut queue = self.0.lock().unwrap();
        let mut all = deferred;
        all.append(&mut queue);
        *queue = all;
    }
}

/// One server's tool sink: the registry `McpClientHandler` refreshes into.
///
/// It runs on whichever thread the handler called it from, so it owns no ECS
/// state: it renames each tool, keeps the generations the handler hands back,
/// and queues the result for the host thread.
struct EcsTools {
    id: String,
    label: String,
    /// The operator's allow-list; empty serves every tool.
    allowed: Vec<String>,
    updates: McpUpdates,
}

impl ManagedToolSink for EcsTools {
    fn add_managed_tools(&self, tools: Vec<DynamicTool>) -> HashMap<String, ManagedToolToken> {
        self.install(tools)
    }

    fn reconcile_managed_tools(
        &self,
        _expected: HashMap<String, ManagedToolToken>,
        tools: Vec<DynamicTool>,
    ) -> HashMap<String, ManagedToolToken> {
        self.install(tools)
    }
}

impl EcsTools {
    /// Renames what this server serves and queues the whole list. The returned
    /// tokens are the generations the handler hands back to the next refresh;
    /// the host thread diffs the list against the entities it already has.
    fn install(&self, tools: Vec<DynamicTool>) -> HashMap<String, ManagedToolToken> {
        let mut installed = HashMap::with_capacity(tools.len());
        let mut served = Vec::with_capacity(tools.len());
        for tool in tools {
            let (mut definition, handler, _) = tool.into_parts();
            let name = definition.name.as_str().to_owned();
            if !self.allowed.is_empty() && !self.allowed.contains(&name) {
                continue;
            }
            let Some(exposed) = exposed_name(&self.label, &self.id, &name) else {
                continue;
            };
            let Ok(tool_name) = ToolName::new(exposed.clone()) else {
                continue;
            };
            definition.name = tool_name;
            installed.insert(exposed.clone(), ManagedToolToken::new());
            served.push((exposed, definition, handler));
        }
        self.updates.push(Update {
            id: self.id.clone(),
            tools: served,
        });
        installed
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<McpServers>()
        .init_resource::<HostToolNames>()
        .init_resource::<Connecting>()
        .init_resource::<McpEvents>()
        .init_resource::<McpUpdates>()
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
        // The host is going away: close every server so no child outlives it.
        let ids: Vec<String> = world.resource::<McpServers>().0.keys().cloned().collect();
        for id in ids {
            retire_one(world, &id);
        }
        return;
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) else {
        return;
    };
    let events = drain(world);
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

    for event in events {
        match event {
            Event::Connected {
                id,
                revision,
                connection,
            } => {
                world.resource_mut::<Connecting>().0.remove(&id);
                let Some((_, want)) = wanted.iter().find(|(wanted, _)| *wanted == id) else {
                    // The setting went away while the task ran.
                    runtime.spawn(async move { connection.close().await });
                    continue;
                };
                if want.revision != revision {
                    runtime.spawn(async move { connection.close().await });
                    continue;
                }
                attach(world, &id, revision, connection);
            }
            Event::Failed { id, reason } => {
                world.resource_mut::<Connecting>().0.remove(&id);
                tracing::warn!(connection = %id, "the MCP server did not connect: {reason}");
            }
        }
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
                    .is_some_and(|(_, want)| want.revision != server.revision)
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in retire {
        retire_one(world, &id);
    }

    apply_updates(world);

    let sender = world.resource::<McpEvents>().sender.clone();
    let updates = world.resource::<McpUpdates>().clone();
    for (id, want) in wanted {
        if world.resource::<McpServers>().0.contains_key(&id)
            || world.resource::<Connecting>().0.contains(&id)
        {
            continue;
        }
        world.resource_mut::<Connecting>().0.insert(id.clone());
        let sender = sender.clone();
        let updates = updates.clone();
        runtime.spawn(async move {
            let event = match open(&id, &want, updates).await {
                Ok((connection, revision)) => Event::Connected {
                    id,
                    revision,
                    connection,
                },
                Err(reason) => Event::Failed { id, reason },
            };
            let _ = sender.send(event).await;
        });
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

/// Opens one server and lists its tools into `updates`.
async fn open(
    id: &str,
    want: &Desired,
    updates: McpUpdates,
) -> Result<(McpConnection, u64), String> {
    let sink = EcsTools {
        id: id.to_owned(),
        label: want.name.clone(),
        allowed: want.allowed.clone(),
        updates,
    };
    let handler = McpClientHandler::new(ClientConfig::default(), sink);
    let connection = if !want.command.is_empty() {
        // A stdio server is a child process the host owns.
        let args = want.args.clone();
        let mut wrap =
            process_wrap::tokio::CommandWrap::with_new(Path::new(&want.command), |cmd| {
                cmd.args(&args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped());
            });
        // The same lifecycle the ACP child gets: one process group, killed on
        // drop, so a host that exits does not orphan the server.
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
        let mut config = StreamableHttpClientTransportConfig::default();
        config.uri = want.endpoint.clone().into();
        let worker = StreamableHttpClientWorker::<reqwest::Client>::new(client, config);
        let transport = StreamableHttpClientTransport::<reqwest::Client>::spawn(worker);
        handler
            .connect(transport)
            .await
            .map_err(|error| error.to_string())?
    } else {
        return Err("the MCP connection names neither a command nor an endpoint".to_owned());
    };
    Ok((connection, want.revision))
}

/// The managed registry owns generations and liveness. ECS owns exposed names.
fn attach(world: &mut World, id: &str, revision: u64, connection: McpConnection) {
    world.resource_mut::<McpServers>().0.insert(
        id.to_owned(),
        Server {
            connection,
            revision,
            tools: HashMap::new(),
        },
    );
}

/// Applies the tool lists the handlers pushed: despawns what a server dropped
/// and spawns what it added.
fn apply_updates(world: &mut World) {
    let updates = world.resource::<McpUpdates>().take();
    let mut deferred = Vec::new();
    let mut changed = false;
    for update in updates {
        let Some(mut server) = world.resource_mut::<McpServers>().0.remove(&update.id) else {
            // A connect task's first list can land before the frame attaches
            // its connection. Keep it only while that connect is still
            // running: a server the settings dropped leaves nothing behind.
            if world.resource::<Connecting>().0.contains(&update.id) {
                deferred.push(update);
            }
            continue;
        };
        // The list is the server's whole set, so a name it no longer carries
        // is one this host must stop offering.
        let served: HashSet<&str> = update
            .tools
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect();
        let stale: Vec<String> = server
            .tools
            .keys()
            .filter(|name| !served.contains(name.as_str()))
            .cloned()
            .collect();
        for name in stale {
            if let Some(entity) = server.tools.remove(&name) {
                if let Ok(entity) = world.get_entity_mut(entity) {
                    entity.despawn();
                }
                changed = true;
            }
        }
        for (name, definition, handler) in update.tools {
            if server.tools.contains_key(&name) {
                continue;
            }
            if let Some(entity) = world.spawn_tool(definition, handler, ToolOptions::default()) {
                server.tools.insert(name, entity);
                changed = true;
            }
        }
        world
            .resource_mut::<McpServers>()
            .0
            .insert(update.id, server);
    }
    world.resource::<McpUpdates>().restore(deferred);
    if changed {
        sync_names(world);
    }
}

fn sync_names(world: &mut World) {
    let live = world
        .resource::<McpServers>()
        .0
        .values()
        .flat_map(|server| server.tools.keys().cloned())
        .collect();
    world.resource_mut::<HostToolNames>().0 = live;
    if let Err(error) = crate::host::client::dynamic::sync_tools(world) {
        tracing::warn!(%error, "the MCP tools are not offered");
    }
}

/// Closes one server and removes the tools it contributed.
fn retire_one(world: &mut World, id: &str) {
    let Some(server) = world.resource_mut::<McpServers>().0.remove(id) else {
        return;
    };
    for entity in server.tools.into_values() {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
        }
    }
    sync_names(world);
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

    /// A server that serves one text tool over an in-process duplex pair, so
    /// the whole client path runs without a child process or a network.
    struct Echo {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        listed: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
                        "Echo the input back",
                        serde_json::Map::new(),
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

    /// The whole path: a listed MCP tool is attached to the real host app,
    /// offered to the parent, passes the gate and runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_mcp_tool_runs_through_the_gate() {
        use crate::host::tests::Harness;
        use rig_core::completion::Message;
        use rig_core::message::{ToolResultContent, UserContent};
        use rig_core::test_utils::MockStreamEvent;
        use rig_rmcp::rmcp::{
            RoleClient, RoleServer, service::serve_directly, transport::async_rw::AsyncRwTransport,
        };

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let server_calls = calls.clone();
        let listed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let server_listed = listed.clone();
        let (peer_sender, peer_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let transport = AsyncRwTransport::<RoleServer, _, _>::new(server_read, server_write);
            let running = serve_directly(
                Echo {
                    calls: server_calls,
                    listed: server_listed,
                },
                transport,
                None,
            );
            let _ = peer_sender.send(running.peer().clone());
            let _ = running.waiting().await;
        });
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(client_read, client_write);
        let updates = McpUpdates::default();
        let sink = EcsTools {
            id: "c1".into(),
            label: "notes".into(),
            allowed: Vec::new(),
            updates: updates.clone(),
        };
        let connection = McpClientHandler::new(ClientConfig::default(), sink)
            .connect(transport)
            .await
            .expect("the server lists its tools");

        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call("call-1", "mcp_notes_echo", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("呼んだよ"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let world = harness.app.world_mut();
        world.init_resource::<McpServers>();
        world.init_resource::<HostToolNames>();
        world.insert_resource(updates);
        attach(world, "c1", 1, connection);
        apply_updates(world);
        assert!(
            world
                .resource::<HostToolNames>()
                .0
                .contains("mcp_notes_echo")
        );

        harness
            .accept("k1", "ノート読んで")
            .expect("the message is accepted");
        harness.run_until(|world| {
            world
                .query::<&Conversation>()
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
        listed.store(false, std::sync::atomic::Ordering::SeqCst);
        peer_receiver
            .await
            .unwrap()
            .notify_tool_list_changed()
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let world = harness.app.world_mut();
                apply_updates(world);
                if !world
                    .resource::<HostToolNames>()
                    .0
                    .contains("mcp_notes_echo")
                {
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
