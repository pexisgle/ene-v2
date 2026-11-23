mod supervisor_tests;

use super::projection::*;
use super::supervisor::*;
use super::*;
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use rig_core::message::ToolName;
use rig_core::tool::{DynamicTool, ToolContext, ToolOutput};
use rig_ecs::prelude::ToolDef;
use rig_rmcp::{McpClientHandler, McpConnection, rmcp::model::ClientConfig};
use std::collections::BTreeMap;

pub(super) fn want(name: &str) -> Desired {
    Desired {
        revision: 1,
        credential_generation: None,
        name: name.into(),
        command: String::new(),
        args: Vec::new(),
        cwd: String::new(),
        env: BTreeMap::new(),
        headers: BTreeMap::new(),
        auth_header: String::new(),
        secret_env_var: String::new(),
        endpoint: String::new(),
        secret: None,
        allowed: Vec::new(),
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
pub(super) struct PendingPeer {
    marker: std::path::PathBuf,
    process: Option<(rustix::process::Pid, rustix::fd::OwnedFd)>,
}

#[cfg(target_os = "linux")]
impl PendingPeer {
    pub(super) fn new(root: &std::path::Path, name: &str) -> Self {
        Self {
            marker: root.join(name),
            process: None,
        }
    }

    pub(super) fn config(&self) -> crate::host::settings::McpConnection {
        crate::host::settings::McpConnection {
            endpoint: Endpoint {
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
            ..Default::default()
        }
    }

    pub(super) async fn started(&mut self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&self.marker)
                    .ok()
                    .and_then(|text| text.parse::<i32>().ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
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

    pub(super) async fn stopped(&mut self, action: &str) {
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
    use crate::host::resources::HostState;

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
        store
            .settings_repo()
            .update_settings(1, &[(row.clone(), 0)])
            .unwrap();
        let mut world = World::new();
        world.insert_resource(store.clone());
        world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        world.init_resource::<HostState>();
        world.init_resource::<McpSupervisor>();
        sync(&mut world);
        peer.started().await;
        assert!(world.resource::<McpServers>().0.is_empty());
        let old_launch = world.resource::<Connecting>().0[&row.id].launch.clone();
        let mut replacement = PendingPeer::new(root.path(), "replacement.pid");
        match action {
            "disable" => {
                row.enabled = false;
                store
                    .settings_repo()
                    .update_settings(2, &[(row.clone(), 1)])
                    .unwrap();
            }
            "revision" => {
                row.config = ConnectionConfig::Mcp(replacement.config());
                store
                    .settings_repo()
                    .update_settings(2, &[(row.clone(), 1)])
                    .unwrap();
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
                    config_error: false,
                },
                &tokio::runtime::Handle::current(),
            );
            assert_eq!(world.resource::<Connecting>().0[&row.id].launch, new_launch);
            assert!(
                rustix::process::test_kill_process(replacement.process.as_ref().unwrap().0).is_ok()
            );
            world.resource_mut::<HostState>().closing = true;
            sync(&mut world);
            replacement.stopped("replacement shutdown").await;
        }
        assert!(world.resource::<Connecting>().0.is_empty());
    }
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
    let mut query = world.query::<&McpTool>();
    query.iter(world).filter(|tool| tool.server == id).count()
}
