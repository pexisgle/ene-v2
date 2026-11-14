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
    resources::{HostRuntime, HostToolNames},
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::ToolDefinition,
    effect::{Answered, EffectKind, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    serve::{Dispatch, ErasedHandler, Reply, Serve},
    tool::{ToolErrorKind, ToolOutput, ToolResult},
};
use rig_ecs::prelude::*;
use rig_rmcp::{
    McpConnection,
    rmcp::{
        ServiceExt,
        model::{CallToolRequestParams, CallToolResult, ClientConfig},
        service::ServerSink,
        transport::{
            child_process::TokioChildProcess,
            streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientWorker},
        },
    },
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
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
    /// The model-facing names these tools hold, so a retire can drop them.
    names: Vec<String>,
    tools: Vec<Entity>,
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
        tools: Vec<rig_rmcp::McpTool>,
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

pub(crate) fn register(app: &mut App) {
    app.init_resource::<McpServers>()
        .init_resource::<HostToolNames>()
        .init_resource::<Connecting>()
        .init_resource::<McpEvents>()
        .add_systems(Update, sync.after(AgentLoop));
}

/// What one settings row asks the host to connect to.
struct Desired {
    revision: u64,
    name: String,
    command: String,
    args: Vec<String>,
    endpoint: String,
    allowed: Vec<String>,
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
/// attaches the tools a connect task listed.
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
    let wanted = desired(&store);

    for event in events {
        match event {
            Event::Connected {
                id,
                revision,
                connection,
                tools,
            } => {
                world.resource_mut::<Connecting>().0.remove(&id);
                let Some((_, want)) = wanted.iter().find(|(wanted, _)| *wanted == id) else {
                    // The setting went away while the task ran.
                    runtime.spawn(async move { connection.close().await });
                    continue;
                };
                attach(
                    world,
                    &id,
                    revision,
                    &want.name,
                    connection,
                    tools,
                    &want.allowed,
                );
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
            !wanted_ids.contains(id.as_str())
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

    let sender = world.resource::<McpEvents>().sender.clone();
    for (id, want) in wanted {
        if world.resource::<McpServers>().0.contains_key(&id)
            || world.resource::<Connecting>().0.contains(&id)
        {
            continue;
        }
        world.resource_mut::<Connecting>().0.insert(id.clone());
        let sender = sender.clone();
        runtime.spawn(async move {
            let event = match open(&want).await {
                Ok((connection, tools)) => Event::Connected {
                    id,
                    revision: want.revision,
                    connection,
                    tools,
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

/// Opens one server and lists its tools.
async fn open(want: &Desired) -> Result<(McpConnection, Vec<rig_rmcp::McpTool>), String> {
    let service = if !want.command.is_empty() {
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
        ClientConfig::default()
            .serve(transport)
            .await
            .map_err(|error| error.to_string())?
    } else if !want.endpoint.is_empty() {
        let worker =
            StreamableHttpClientWorker::<reqwest::Client>::new_simple(want.endpoint.clone());
        let transport = StreamableHttpClientTransport::<reqwest::Client>::spawn(worker);
        ClientConfig::default()
            .serve(transport)
            .await
            .map_err(|error| error.to_string())?
    } else {
        return Err("the MCP connection names neither a command nor an endpoint".to_owned());
    };
    bind(service).await
}

/// Binds one connected client and lists what it serves.
async fn bind<S>(
    service: rig_rmcp::rmcp::service::RunningService<rig_rmcp::rmcp::RoleClient, S>,
) -> Result<(McpConnection, Vec<rig_rmcp::McpTool>), String>
where
    S: rig_rmcp::rmcp::Service<rig_rmcp::rmcp::RoleClient>,
    S: Send + Sync + 'static,
{
    let connection = McpConnection::new(service);
    // rig-rmcp binds each listed tool to this connection, which keeps the
    // server alive for as long as its tools are registered.
    let tools = connection
        .tools()
        .await
        .map_err(|error| error.to_string())?;
    Ok((connection, tools))
}

/// Registers the tools `allowed` names, or every tool it lists when the
/// filter is empty.
fn attach(
    world: &mut World,
    id: &str,
    revision: u64,
    label: &str,
    connection: McpConnection,
    tools: Vec<rig_rmcp::McpTool>,
    allowed: &[String],
) {
    let peer = connection.peer().clone();
    let mut entities = Vec::new();
    let mut names = Vec::new();
    for tool in tools {
        let definition = tool.definition();
        let name = definition.name.to_string();
        if !allowed.is_empty() && !allowed.contains(&name) {
            continue;
        }
        let Some(exposed) = exposed_name(label, id, &name) else {
            continue;
        };
        let description = definition
            .description
            .as_ref()
            .map_or_else(|| format!("MCP tool `{name}`"), ToString::to_string);
        let parameters = Value::Object((*definition.input_schema).clone());
        let handler = McpToolHandler {
            peer: peer.clone(),
            mcp_name: name,
            exposed: exposed.clone(),
            description: description.clone(),
            parameters: parameters.clone(),
        };
        let Some(tool_name) = rig_core::message::ToolName::new(exposed.clone()).ok() else {
            continue;
        };
        let definition = ToolDefinition::new(tool_name, description, parameters);
        if let Some(entity) = world.spawn_tool(
            definition,
            ErasedHandler::new(handler),
            ToolOptions::default(),
        ) {
            entities.push(entity);
            names.push(exposed);
        }
    }
    // The gate passes these and a parent is offered them, so both lists have
    // to learn the names before the next turn.
    {
        let mut host = world.resource_mut::<HostToolNames>();
        host.0.extend(names.iter().cloned());
    }
    if let Err(error) = crate::host::client::dynamic::sync_tools(world) {
        tracing::warn!(connection = %id, "the MCP tools are not offered: {error}");
    }
    world.resource_mut::<McpServers>().0.insert(
        id.to_owned(),
        Server {
            connection,
            revision,
            names,
            tools: entities,
        },
    );
}

/// Closes one server and removes the tools it contributed.
fn retire_one(world: &mut World, id: &str) {
    let Some(server) = world.resource_mut::<McpServers>().0.remove(id) else {
        return;
    };
    for entity in server.tools {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
        }
    }
    // Rebuild from what is still connected: two servers may have contributed
    // the same tool name, and only the ones still registered belong.
    {
        let live: std::collections::HashSet<String> = world
            .resource::<McpServers>()
            .0
            .values()
            .flat_map(|server| server.names.iter().cloned())
            .collect();
        world.resource_mut::<HostToolNames>().0 = live;
    }
    if let Err(error) = crate::host::client::dynamic::sync_tools(world) {
        tracing::warn!(connection = %id, "the retired MCP tools may still be offered: {error}");
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

/// One MCP tool as the host serves it.
struct McpToolHandler {
    peer: ServerSink,
    mcp_name: String,
    exposed: String,
    description: String,
    parameters: Value,
}

impl Serve for McpToolHandler {
    type Family = family::Tool;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            &self.exposed,
            self.description.clone(),
            self.parameters.clone(),
        )
    }

    async fn serve(&self, kind: EffectKind, _dispatch: Dispatch) -> Reply {
        let EffectKind::ToolCall { args, .. } = kind else {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "an MCP tool was dispatched a non-tool effect",
            )));
        };
        let arguments = match serde_json::from_str::<serde_json::Map<String, Value>>(&args) {
            Ok(arguments) => arguments,
            Err(error) => {
                return Reply::Outcome(Err(ErrorReport::new(
                    ErrorKind::Tool(ToolErrorKind::InvalidArgs),
                    format!("the MCP tool arguments are not a JSON object: {error}"),
                )));
            }
        };
        let mut params = CallToolRequestParams::new(self.mcp_name.clone());
        params.arguments = Some(arguments);
        match self.peer.call_tool(params).await {
            Ok(result) => match render(&result) {
                Ok(text) => Reply::Outcome(Ok(Answered::ToolResult {
                    result: ToolResult::success(ToolOutput::text(text)),
                })),
                Err(reason) => Reply::Outcome(Err(ErrorReport::new(
                    ErrorKind::Tool(ToolErrorKind::Provider),
                    reason,
                ))),
            },
            Err(error) => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::Tool(ToolErrorKind::Provider),
                format!("the MCP tool `{}` failed: {error}", self.mcp_name),
            ))),
        }
    }
}

/// The text one call answered with. A structured answer is passed through as
/// JSON; otherwise the text blocks are joined.
fn render(result: &CallToolResult) -> std::result::Result<String, String> {
    if result.is_error == Some(true) {
        return Err(text_of(result));
    }
    if let Some(structured) = result.structured_content.as_ref() {
        return Ok(structured.to_string());
    }
    Ok(text_of(result))
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
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

    /// The model-facing name carries the server, so two servers that offer the
    /// same tool keep two tools. A part the provider grammar cannot carry is
    /// refused rather than rewritten onto another tool.
    #[test]
    fn a_tool_name_carries_the_server_and_is_refused_when_it_cannot_be_spelled() {
        assert_eq!(
            exposed_name("notes", "c1", "read_file").as_deref(),
            Some("mcp_notes_read_file")
        );
        assert_eq!(
            exposed_name("notes", "c1", "read-file").as_deref(),
            Some("mcp_notes_read-file")
        );
        assert_ne!(
            exposed_name("notes", "c1", "read"),
            exposed_name("work", "c2", "read")
        );
        // A server name the grammar cannot carry falls back to its id.
        assert_eq!(
            exposed_name("my server", "abcdef012345", "read").as_deref(),
            Some("mcp_abcdef01_read")
        );
        assert_eq!(exposed_name("notes", "c1", "read.file"), None);
        assert_eq!(exposed_name("notes", "c1", "read file"), None);
        let long = "x".repeat(70);
        assert_eq!(exposed_name("notes", "c1", &long), None);
    }

    /// A setting that names neither a command nor an endpoint cannot be
    /// opened; it is reported instead of waiting on a transport.
    #[tokio::test]
    async fn a_server_without_a_transport_is_refused() {
        let want = Desired {
            revision: 1,
            name: "notes".into(),
            command: String::new(),
            args: Vec::new(),
            endpoint: String::new(),
            allowed: Vec::new(),
        };
        match open(&want).await {
            Ok(_) => panic!("a server without a transport was opened"),
            Err(error) => assert!(
                error.contains("neither a command nor an endpoint"),
                "{error}"
            ),
        }
    }

    /// A server that serves one text tool over an in-process duplex pair, so
    /// the whole client path runs without a child process or a network.
    struct Echo(std::sync::Arc<std::sync::atomic::AtomicUsize>);

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
                vec![rig_rmcp::rmcp::model::Tool::new(
                    "echo",
                    "Echo the input back",
                    serde_json::Map::new(),
                )],
            ))
        }

        async fn call_tool(
            &self,
            request: rig_rmcp::rmcp::model::CallToolRequestParams,
            _context: rig_rmcp::rmcp::service::RequestContext<rig_rmcp::rmcp::RoleServer>,
        ) -> Result<rig_rmcp::rmcp::model::CallToolResponse, rig_rmcp::rmcp::ErrorData> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(rig_rmcp::rmcp::model::CallToolResponse::Complete(
                rig_rmcp::rmcp::model::CallToolResult::success(vec![
                    rig_rmcp::rmcp::model::ContentBlock::text(format!("called {}", request.name)),
                ]),
            ))
        }
    }

    /// The path the review found broken twice: connect, list, register, and
    /// call. It runs over a duplex pair so it needs no server binary.
    #[tokio::test]
    async fn a_duplex_server_is_listed_and_called() {
        use rig_core::effect::{EffectId, EffectKind};
        use rig_core::serve::Dispatch;
        use rig_rmcp::rmcp::{
            RoleClient, RoleServer, ServiceExt, service::serve_directly,
            transport::async_rw::AsyncRwTransport,
        };

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            let transport = AsyncRwTransport::<RoleServer, _, _>::new(server_read, server_write);
            let running = serve_directly(Echo(server_calls), transport, None);
            let _ = running.waiting().await;
        });

        let transport = AsyncRwTransport::<RoleClient, _, _>::new(client_read, client_write);
        let service = rig_rmcp::rmcp::model::ClientConfig::default()
            .serve(transport)
            .await
            .expect("the client connects to the duplex server");
        let (connection, tools) = bind(service).await.expect("the server lists its tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "echo");

        let handler = McpToolHandler {
            peer: connection.peer().clone(),
            mcp_name: "echo".to_owned(),
            exposed: "mcp_notes_echo".to_owned(),
            description: "Echo the input back".to_owned(),
            parameters: Value::Object(serde_json::Map::new()),
        };
        let reply = handler
            .serve(
                EffectKind::ToolCall {
                    name: "mcp_notes_echo".to_owned(),
                    args: "{}".to_owned(),
                },
                Dispatch::new(EffectId::from_raw(0), false),
            )
            .await;
        let Reply::Outcome(Ok(Answered::ToolResult { result })) = reply else {
            panic!("the MCP tool did not answer");
        };
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the call reached the server"
        );
        let _ = result;

        connection.close().await;
        server.abort();
    }

    /// The whole path the review found broken: a listed MCP tool is attached to
    /// the real host app, offered to the parent, passes the gate, runs, and its
    /// answer reaches the conversation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_mcp_tool_runs_through_the_gate() {
        use crate::host::tests::Harness;
        use rig_core::test_utils::MockStreamEvent;
        use rig_rmcp::rmcp::{
            RoleClient, RoleServer, ServiceExt, service::serve_directly,
            transport::async_rw::AsyncRwTransport,
        };

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            let transport = AsyncRwTransport::<RoleServer, _, _>::new(server_read, server_write);
            let running = serve_directly(Echo(server_calls), transport, None);
            let _ = running.waiting().await;
        });
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(client_read, client_write);
        let service = rig_rmcp::rmcp::model::ClientConfig::default()
            .serve(transport)
            .await
            .expect("the client connects to the duplex server");
        let (connection, tools) = bind(service).await.expect("the server lists its tools");

        let harness = Harness::new([
            vec![
                MockStreamEvent::tool_call("call-1", "mcp_notes_echo", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("呼んだよ"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let mut harness = harness;
        let world = harness.app.world_mut();
        world.init_resource::<McpServers>();
        world.init_resource::<HostToolNames>();
        attach(world, "c1", 1, "notes", connection, tools, &[]);
        assert!(
            world
                .resource::<HostToolNames>()
                .0
                .contains("mcp_notes_echo")
        );

        harness
            .accept("k1", "ノート読んで")
            .expect("the message is accepted");
        // The tool call runs on the app; drive it until the server sees it.
        let started = std::time::Instant::now();
        while calls.load(std::sync::atomic::Ordering::SeqCst) == 0
            && started.elapsed() < std::time::Duration::from_secs(5)
        {
            harness.app.update();
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the model's call reached the MCP server"
        );
        server.abort();
    }
}
