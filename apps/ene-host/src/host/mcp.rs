//! MCP servers as ordinary tools (tech-stack §B).
//!
//! One enabled `Mcp` connection is one server. The host connects on its
//! runtime, lists the server's tools and registers each allowed one like any
//! other tool, so the agent does not care whether a tool is built in, served by
//! a client or served by an MCP process.
//!
//! The tools belong to the operator's own configuration rather than to a
//! client's declaration, so they run without an approval card. The gate stays
//! in charge of what a *client* may ask the host to do; it never sees these.
//!
//! A server that is disabled, deleted, re-configured or disconnected loses its
//! tools: the connection closes and the tool entities despawn.
use crate::host::{
    db::Store,
    resources::HostRuntime,
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
        model::{CallToolRequestParams, CallToolResult, ClientConfig, Tool as McpTool},
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
        tools: Vec<McpTool>,
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
        .init_resource::<Connecting>()
        .init_resource::<McpEvents>()
        .add_systems(Update, sync.after(AgentLoop));
}

/// What one settings row asks the host to connect to.
struct Desired {
    revision: u64,
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
                attach(world, &id, revision, connection, tools, &want.allowed);
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
async fn open(want: &Desired) -> Result<(McpConnection, Vec<McpTool>), String> {
    let service = if !want.command.is_empty() {
        // A stdio server is a child process the host owns.
        let args = want.args.clone();
        let wrap = process_wrap::tokio::CommandWrap::with_new(Path::new(&want.command), |cmd| {
            cmd.args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
        });
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
    let connection = McpConnection::new(service);
    let listed = connection
        .peer()
        .list_tools(None)
        .await
        .map_err(|error| error.to_string())?;
    Ok((connection, listed.tools))
}

/// Registers the tools `allowed` names, or every tool it lists when the
/// filter is empty.
fn attach(
    world: &mut World,
    id: &str,
    revision: u64,
    connection: McpConnection,
    tools: Vec<McpTool>,
    allowed: &[String],
) {
    let peer = connection.peer().clone();
    let mut entities = Vec::new();
    for tool in tools {
        let name = tool.name.to_string();
        if !allowed.is_empty() && !allowed.contains(&name) {
            continue;
        }
        let Some(exposed) = exposed_name(id, &name) else {
            continue;
        };
        let description = tool
            .description
            .as_ref()
            .map_or_else(|| format!("MCP tool `{name}`"), ToString::to_string);
        let parameters = Value::Object((*tool.input_schema).clone());
        let handler = McpToolHandler {
            peer: peer.clone(),
            mcp_name: name,
            exposed: exposed.clone(),
            description: description.clone(),
            parameters: parameters.clone(),
        };
        let Some(name) = rig_core::message::ToolName::new(exposed).ok() else {
            continue;
        };
        let definition = ToolDefinition::new(name, description, parameters);
        if let Some(entity) = world.spawn_tool(
            definition,
            ErasedHandler::new(handler),
            ToolOptions::default(),
        ) {
            entities.push(entity);
        }
    }
    world.resource_mut::<McpServers>().0.insert(
        id.to_owned(),
        Server {
            connection,
            revision,
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
    // Closing waits for the transport, so it runs off the host thread.
    if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
        let connection = server.connection;
        runtime.spawn(async move { connection.close().await });
    }
}

/// The model-facing name for a server's tool. A name the provider grammar
/// cannot carry, or one the host already reserves, is refused rather than
/// silently renamed onto another tool.
fn exposed_name(connection: &str, tool: &str) -> Option<String> {
    let mut name = String::with_capacity(tool.len() + 4);
    name.push_str("mcp_");
    for character in tool.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            name.push(character);
        } else {
            return None;
        }
    }
    if name.len() > 64 || name.starts_with("mcp_") && name.len() <= 4 {
        tracing::warn!(connection = %connection, tool = %tool, "the MCP tool name is not usable");
        return None;
    }
    Some(name)
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

    /// The model-facing name is namespaced, and a name the provider grammar
    /// cannot carry is refused rather than rewritten onto another tool.
    #[test]
    fn a_tool_name_is_namespaced_and_refused_when_it_cannot_be_carried() {
        assert_eq!(
            exposed_name("c1", "read_file").as_deref(),
            Some("mcp_read_file")
        );
        assert_eq!(
            exposed_name("c1", "read-file").as_deref(),
            Some("mcp_read-file")
        );
        assert_eq!(exposed_name("c1", "read.file"), None);
        assert_eq!(exposed_name("c1", "read file"), None);
        let long = "x".repeat(70);
        assert_eq!(exposed_name("c1", &long), None);
    }

    /// A setting that names neither a command nor an endpoint cannot be
    /// opened; it is reported instead of waiting on a transport.
    #[tokio::test]
    async fn a_server_without_a_transport_is_refused() {
        let want = Desired {
            revision: 1,
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
}
