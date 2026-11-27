#[cfg(target_os = "linux")]
mod pending_tests;
mod supervisor_tests;

use super::*;
use super::{settings::Desired, startup::open, supervisor::*};
use crate::host::resources::{ExternalToolOwner, HostExternalTool};
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use rig_core::message::ToolName;
use rig_core::tool::{DynamicTool, ToolContext, ToolOutput};
use rig_ecs::prelude::ToolDef;
use rig_rmcp::{McpClientHandler, McpConnection, rmcp::model::ClientConfig};
use tokio_util::sync::CancellationToken;

pub(super) const STAMP: ConnectionStamp = ConnectionStamp {
    revision: 2,
    created_revision: 2,
    credential_generation: 0,
};

pub(super) fn want(name: &str) -> Desired {
    Desired {
        stamp: STAMP,
        name: name.into(),
        config: Default::default(),
        credential_state: Default::default(),
    }
}

/// A server that serves one text tool over an in-process duplex pair, so
/// the whole client path runs without a child process or a network.
#[derive(Clone)]
pub(super) struct Echo {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    listed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    generation: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Echo {
    pub(super) fn new() -> Self {
        Self {
            calls: Default::default(),
            listed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            generation: Default::default(),
        }
    }
}

pub(super) async fn connect_echo(
    echo: Echo,
) -> (
    McpConnection,
    ToolServerHandle,
    rig_rmcp::rmcp::service::Peer<rig_rmcp::rmcp::RoleServer>,
    tokio::task::JoinHandle<()>,
) {
    Box::pin(async move {
        use rig_rmcp::rmcp::{
            RoleClient, RoleServer, ServiceExt, transport::async_rw::AsyncRwTransport,
        };
        let (client_read, server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let (peer_sender, peer_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let transport = AsyncRwTransport::<RoleServer, _, _>::new(server_read, server_write);
            let running = echo.serve(transport).await.unwrap();
            let _discarded = peer_sender.send(running.peer().clone());
            let _discarded = running.waiting().await;
        });
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(client_read, client_write);
        let registry = ToolServer::new().run();
        let connection = Box::pin(
            McpClientHandler::new(ClientConfig::default(), registry.clone()).connect(transport),
        )
        .await
        .expect("the server lists its tools");
        (connection, registry, peer_receiver.await.unwrap(), server)
    })
    .await
}

impl rig_rmcp::rmcp::ServerHandler for Echo {
    fn get_info(&self) -> rig_rmcp::rmcp::model::ServerConfig {
        use rig_rmcp::rmcp::model::{ServerCapabilities, ServerConfig};
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
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
    desired.config.endpoint.endpoint = format!("http://{address}/mcp");
    let registry = ToolServer::new().run();
    let stop = CancellationToken::new();
    let connection = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        open(&desired.config, None, registry.clone(), &stop),
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
    desired.config.endpoint.endpoint = format!("http://{address}/redirect");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            open(&desired.config, None, ToolServer::new().run(), &stop)
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

pub(super) fn local_tool(name: &str, generation: usize) -> DynamicTool {
    DynamicTool::new(
        ToolName::new(name).unwrap(),
        format!("generation {generation}"),
        serde_json::json!({"type":"object","properties":{"version":{"const":generation}}}),
        move |_| Box::pin(async move { Ok(ToolOutput::text(generation.to_string())) }),
    )
}

pub(super) fn tool_entity(world: &mut World, name: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &ToolDef)>();
    query
        .iter(world)
        .find(|(_, definition)| definition.0.name.as_str() == name)
        .map(|(entity, _)| entity)
}

pub(super) fn server_tools(world: &mut World, id: &str) -> usize {
    world
        .query::<&HostExternalTool>()
        .iter(world)
        .filter(
            |tool| matches!(&tool.owner, ExternalToolOwner::Mcp(binding) if binding.server == id),
        )
        .count()
}
