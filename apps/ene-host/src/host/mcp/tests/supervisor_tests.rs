use super::*;
use crate::host::mcp::projection::*;
use crate::host::mcp::supervisor::{Event, *};
use crate::host::mcp::*;
use crate::host::{
    db::Store,
    model::Credentials,
    resources::{HostRuntime, WorkAgent},
    settings::{Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint},
};
use rig_core::tool::{ManagedToolSink, ToolContext};
use rig_ecs::prelude::{ToolDef, ToolOptions};
use std::collections::BTreeMap;
use tokio_util::task::AbortOnDropHandle;

/// Asserts the pending connect for `c1` still carries `launch`.
fn assert_pending_launch(supervisor: &McpSupervisor, launch: &Launch) {
    assert_eq!(
        supervisor
            .connecting
            .0
            .get("c1")
            .map(|pending| &pending.launch),
        Some(launch)
    );
}

fn attach_now(
    world: &mut World,
    id: &str,
    launch: Launch,
    connection: rig_rmcp::McpConnection,
    registry: rig_agent::tool::server::ToolServerHandle,
    want: &Desired,
) {
    let previous = world.resource::<Mcp>().lock().servers.0.insert(
        id.to_owned(),
        Server {
            launch,
            connection,
            registry,
            label: want.name.clone(),
            allowed: want.allowed.clone(),
        },
    );
    assert!(previous.is_none());
}

fn retire_one_now(world: &mut World, id: &str) {
    let server = world.resource::<Mcp>().lock().servers.0.remove(id).unwrap();
    close_connection(server.connection, &world.resource::<HostRuntime>().0);
    project_servers_now(world);
}

fn project_servers_now(world: &mut World) {
    let mcp = world.resource::<Mcp>().clone();
    let snapshot = snapshot_tools(&mcp.lock().servers);
    project_servers(world, snapshot);
}

fn project(
    world: &mut World,
    id: &str,
    server: &Server,
    catalog: &rig_agent::tool::ToolCatalog,
) -> bool {
    project_servers(
        world,
        [(id.to_owned(), collect_server_tools(id, server, catalog))].into(),
    )
}

#[tokio::test]
async fn registry_refresh_replaces_handlers_and_keeps_pinned_calls() {
    let echo = Echo::new();
    echo.listed
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (connection, registry, _, task) = connect_echo(echo).await;
    let expected = registry.add_managed_tools(vec![local_tool("echo", 1)]);
    let pinned = registry.snapshot();
    let server = Server {
        connection: connection.clone(),
        launch: Launch {
            revision: 1,
            credential_generation: None,
            owner: registry.owner(),
        },
        registry: registry.clone(),
        label: "notes".into(),
        allowed: Vec::new(),
    };
    let mut world = World::new();
    assert!(project(&mut world, "c1", &server, &pinned));
    let old_entity = tool_entity(&mut world, "mcp_notes_echo").unwrap();
    let refreshed = registry.reconcile_managed_tools(expected.clone(), vec![local_tool("echo", 2)]);
    let current = registry.snapshot();
    assert!(project(&mut world, "c1", &server, &current));
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
    assert_eq!(
        registry.snapshot().get("echo").unwrap().key(),
        current.get("echo").unwrap().key(),
    );
    registry.reconcile_managed_tools(refreshed, Vec::new());
    assert!(project(&mut world, "c1", &server, &registry.snapshot()));
    assert!(tool_entity(&mut world, "mcp_notes_echo").is_none());
    assert!(world.get_entity(entity).is_err());
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let probe = alive.clone();
    registry.add_managed_tools(vec![
        local_tool("echo", 4)
            .with_liveness(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
    ]);
    assert!(project(&mut world, "c1", &server, &registry.snapshot()));
    alive.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(project(&mut world, "c1", &server, &registry.snapshot()));
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
    world.init_resource::<Mcp>();
    world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
    let (definition, handler, _) = local_tool("mcp_notes_echo", 0).into_parts();
    let foreign = world
        .spawn_tool(definition, handler, ToolOptions::default())
        .unwrap();
    let mut desired = want("notes");
    desired.allowed = vec!["echo".into(), "bad.name".into()];
    attach_now(
        world,
        "c1",
        Launch {
            revision: 1,
            credential_generation: None,
            owner: registry.owner(),
        },
        first.clone(),
        registry.clone(),
        &desired,
    );
    project_servers_now(world);
    assert_eq!(server_tools(world, "c1"), 0);
    assert!(!tool_names(world).contains("mcp_notes_echo"));
    assert!(world.get_entity(foreign).is_ok());
    world.entity_mut(foreign).despawn();
    project_servers_now(world);
    let entity = tool_entity(world, "mcp_notes_echo").unwrap();
    assert!(!tool_names(world).contains("mcp_notes_other"));
    assert_eq!(server_tools(world, "c1"), 1);
    attach_now(
        world,
        "c2",
        Launch {
            revision: 1,
            credential_generation: None,
            owner: other_registry.owner(),
        },
        second.clone(),
        other_registry,
        &want("notes"),
    );
    project_servers_now(world);
    assert_eq!(server_tools(world, "c2"), 0);
    registry.add_managed_tools(vec![local_tool("echo", 3)]);
    project_servers_now(world);
    assert!(world.get_entity(entity).is_err());
    let refreshed = tool_entity(world, "mcp_notes_echo").unwrap();
    assert_eq!(server_tools(world, "c1"), 1);
    assert_eq!(server_tools(world, "c2"), 0);

    retire_one_now(world, "c1");
    assert!(world.get_entity(refreshed).is_err());
    assert_eq!(server_tools(world, "c2"), 1);
    retire_one_now(world, "c2");
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
        credential_generation: None,
        owner: old_registry.owner(),
    };
    let new_launch = Launch {
        revision: 1,
        credential_generation: None,
        owner: new_registry.owner(),
    };
    assert_ne!(old_launch, new_launch);
    let mut harness = crate::host::tests::Harness::new([]);
    let world = harness.app.world_mut();
    world.init_resource::<Mcp>();

    let runtime = tokio::runtime::Handle::current();
    world.insert_resource(HostRuntime(runtime.clone()));
    let mut supervisor = std::mem::take(&mut *world.resource::<Mcp>().lock());
    supervisor.connecting.0.insert(
        "c1".into(),
        Pending {
            launch: new_launch.clone(),
            _task: AbortOnDropHandle::new(runtime.spawn(std::future::pending())),
        },
    );
    let wanted = vec![("c1".into(), want("notes"))];
    accept_event(
        &mut supervisor,
        &wanted,
        Event::Failed {
            id: "c1".into(),
            launch: old_launch.clone(),
            reason: "old failure".into(),
            config_error: false,
        },
        &runtime,
    );
    assert_pending_launch(&supervisor, &new_launch);
    let mut changed_revision = wanted.clone();
    changed_revision[0].1.revision = 2;
    accept_event(
        &mut supervisor,
        &changed_revision,
        Event::Failed {
            id: "c1".into(),
            launch: new_launch.clone(),
            reason: "obsolete revision".into(),
            config_error: false,
        },
        &runtime,
    );
    assert_pending_launch(&supervisor, &new_launch);
    accept_event(
        &mut supervisor,
        &wanted,
        Event::Connected {
            id: "c1".into(),
            launch: new_launch,
            connection: new.clone(),
            registry: new_registry,
        },
        &runtime,
    );
    // accept_event worked on the local copy; publish it before projecting.
    *world.resource::<Mcp>().lock() = std::mem::take(&mut supervisor);
    assert!(world.resource::<Mcp>().lock().connecting.0.is_empty());
    project_servers_now(world);
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
        &mut supervisor,
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
    project_servers_now(world);
    assert_eq!(tool_entity(world, "mcp_notes_echo"), Some(current));
    assert!(!tool_names(world).contains("mcp_notes_late"));
    retire_one_now(world, "c1");
    old_registry.add_managed_tools(vec![local_tool("after_retire", 99)]);
    project_servers_now(world);
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
fn setup_gate_mcp_harness(
    connection: McpConnection,
    registry: ToolServerHandle,
) -> crate::host::tests::Harness {
    use crate::host::agent::CompanionAgent;
    use crate::host::tests::Harness;
    use rig_core::test_utils::MockStreamEvent;
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
    world.init_resource::<Mcp>();
    attach_now(
        world,
        "c1",
        Launch {
            revision: 1,
            credential_generation: None,
            owner: registry.owner(),
        },
        connection,
        registry,
        &want("notes"),
    );
    project_servers_now(world);
    assert!(tool_names(world).contains("mcp_notes_echo"));
    let head_allows_mcp = world
        .query_filtered::<&ToolAccess, With<CompanionAgent>>()
        .iter(world)
        .any(|access| access.allows("mcp_notes_echo"));
    assert!(
        !head_allows_mcp,
        "companion head must not receive general MCP tools"
    );
    harness
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_tool_runs_through_the_gate() {
    use rig_core::completion::Message;
    use rig_core::message::{ToolResultContent, UserContent};
    let echo = Echo::new();
    let calls = echo.calls.clone();
    let listed = echo.listed.clone();
    let generation = echo.generation.clone();
    let (connection, registry, peer, server) = connect_echo(echo).await;

    let mut harness = setup_gate_mcp_harness(connection, registry);
    let companion_id = harness.companion.clone();
    crate::host::work::start(
        harness.app.world_mut(),
        &ene_rpc::StartWorkRequest {
            companion_id,
            instruction: "ノート読んで".into(),
            ..Default::default()
        },
    )
    .expect("work session starts");

    // Without a saved Allow rule or Decision engine allow, Stage::Gate holds
    // the MCP call for explicit approval rather than running unapproved.
    harness.run_until(|world| {
        let hand_allows_mcp = world
            .query_filtered::<&ToolAccess, With<WorkAgent>>()
            .iter(world)
            .any(|access| access.allows("mcp_notes_echo"));
        if !hand_allows_mcp {
            return false;
        }
        !world
            .resource::<Store>()
            .approval_repo()
            .approvals(crate::host::permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "unapproved MCP tool must not execute before approval"
    );
    let pending_id = harness
        .store()
        .approval_repo()
        .approvals(crate::host::permission::ApprovalStatus::Pending)
        .unwrap()[0]
        .request_id
        .clone();
    crate::host::permission::submit(
        harness.app.world_mut(),
        &pending_id,
        crate::host::permission::ApprovalDecision::Approve,
    )
    .unwrap();

    harness.run_until(|world| {
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
    assert_echo_refresh_and_removal(&mut harness, &peer, &listed, &generation).await;
    server.abort();
}

async fn assert_echo_refresh_and_removal(
    harness: &mut crate::host::tests::Harness,
    peer: &rig_rmcp::rmcp::service::Peer<rig_rmcp::rmcp::RoleServer>,
    listed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    generation: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let old_entity = tool_entity(harness.app.world_mut(), "mcp_notes_echo").unwrap();
    generation.store(2, std::sync::atomic::Ordering::SeqCst);
    peer.notify_tool_list_changed().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let world = harness.app.world_mut();
            project_servers_now(world);
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
            project_servers_now(world);
            if !tool_names(world).contains("mcp_notes_echo") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("list_changed removes the tool from the host");
    let world = harness.app.world_mut();
    assert!(
        world
            .query_filtered::<&ToolAccess, With<WorkAgent>>()
            .iter(world)
            .all(|access| !access.allows("mcp_notes_echo"))
    );
}

#[tokio::test]
async fn b17_mcp_backoff_prevents_tight_retry_and_recovers_on_reconnect_request() {
    use axum::{Json, Router, response::IntoResponse, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let fail_requests = Arc::new(AtomicBool::new(true));
    let req_hits = Arc::clone(&hits);
    let req_fail = Arc::clone(&fail_requests);
    let app = Router::new().route(
        "/mcp",
        post(move |Json(request): Json<serde_json::Value>| {
            req_hits.fetch_add(1, Ordering::SeqCst);
            let should_fail = req_fail.load(Ordering::SeqCst);
            async move {
                if should_fail {
                    return http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                let Some(id) = request.get("id") else {
                    return http::StatusCode::ACCEPTED.into_response();
                };
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({
                        "protocolVersion": request["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "loopback", "version": "1"}
                    }),
                    "tools/list" => serde_json::json!({
                        "tools": [{"name": "ping", "description": "Ping", "inputSchema": {"type": "object"}}]
                    }),
                    _ => serde_json::json!({}),
                };
                Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut harness = crate::host::tests::Harness::new([]);
    let rev = harness.store().settings_repo().settings_revision().unwrap();
    let conn = Connection {
        id: "mcp-backoff".into(),
        name: "notes".into(),
        enabled: true,
        kind: ConnectionKind::Mcp,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Mcp(crate::host::settings::McpConnection {
            endpoint: Endpoint {
                adapter: "mcp".into(),
                endpoint: format!("http://{addr}/mcp"),
                options: "{}".into(),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    harness
        .store()
        .settings_repo()
        .update_settings(rev, &[(conn, 0)])
        .unwrap();
    let world = harness.app.world_mut();
    world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
    world.init_resource::<Mcp>();

    // First attempt hits server and enters transient backoff.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            sync(harness.app.world_mut());
            if harness
                .app
                .world()
                .resource::<Mcp>()
                .lock()
                .failures
                .0
                .contains_key("mcp-backoff")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let hits_after_failure = hits.load(Ordering::SeqCst);

    // Stepping sync multiple times inside the 1s backoff window must NOT spawn retries.
    for _ in 0..5 {
        sync(harness.app.world_mut());
    }
    assert_eq!(hits.load(Ordering::SeqCst), hits_after_failure);

    // Recovery via explicit reconnect request connects immediately once healthy.
    fail_requests.store(false, Ordering::SeqCst);
    request_reconnect(
        &mut harness.app.world_mut().resource::<Mcp>().lock(),
        "mcp-backoff",
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            sync(harness.app.world_mut());
            if matches!(
                harness.app.world().resource::<Mcp>().check_state( harness.app.world().get_resource::<Credentials>(), "mcp-backoff", 1),
                CheckState::Connected(ref names) if names == &["ping".to_owned()]
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn f24_http_mcp_sends_auth_header_and_custom_headers_and_recovers_after_credential() {
    use axum::{Json, Router, response::IntoResponse, routing::post};
    let app = Router::new().route(
        "/mcp",
        post(|headers: http::HeaderMap, Json(request): Json<serde_json::Value>| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok());
            let tenant = headers.get("x-tenant").and_then(|v| v.to_str().ok());
            if auth != Some("Bearer mcp-token") || tenant != Some("acme") {
                return http::StatusCode::UNAUTHORIZED.into_response();
            }
            let Some(id) = request.get("id") else {
                return http::StatusCode::ACCEPTED.into_response();
            };
            let result = match request["method"].as_str().unwrap() {
                "initialize" => serde_json::json!({
                    "protocolVersion": request["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "auth-mcp", "version": "1"}
                }),
                "tools/list" => serde_json::json!({
                    "tools": [{"name": "secret_tool", "description": "Auth tool", "inputSchema": {"type": "object"}}]
                }),
                _ => serde_json::json!({}),
            };
            Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut harness = crate::host::tests::Harness::new([]);
    let rev = harness.store().settings_repo().settings_revision().unwrap();
    let mut custom_headers = BTreeMap::new();
    custom_headers.insert("x-tenant".into(), "acme".into());
    let conn = Connection {
        id: "mcp-auth".into(),
        name: "authmcp".into(),
        enabled: true,
        kind: ConnectionKind::Mcp,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Mcp(crate::host::settings::McpConnection {
            endpoint: Endpoint {
                adapter: "mcp".into(),
                endpoint: format!("http://{addr}/mcp"),
                options: "{}".into(),
                ..Default::default()
            },
            headers: custom_headers,
            ..Default::default()
        }),
    };
    harness
        .store()
        .settings_repo()
        .update_settings(rev, &[(conn, 0)])
        .unwrap();
    let world = harness.app.world_mut();
    world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
    world.init_resource::<Mcp>();

    // Without credentials, 401 places the connection on ConfigHold.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            sync(harness.app.world_mut());
            if matches!(
                harness.app.world().resource::<Mcp>().check_state(
                    harness.app.world().get_resource::<Credentials>(),
                    "mcp-auth",
                    1
                ),
                CheckState::Failed(_)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Registering the secret bumps Credentials::generation() without touching settings.revision,
    // automatically clearing ConfigHold and authenticating with Bearer + X-Tenant.
    let creds = harness.app.world().resource::<Credentials>().clone();
    crate::host::model::save_secret(&creds, "mcp-auth", "mcp-token").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            sync(harness.app.world_mut());
            if matches!(
                harness.app.world().resource::<Mcp>().check_state( harness.app.world().get_resource::<Credentials>(), "mcp-auth", 1),
                CheckState::Connected(ref names) if names == &["secret_tool".to_owned()]
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(tool_names(harness.app.world_mut()).contains("mcp_authmcp_secret_tool"));
    server.abort();
}
