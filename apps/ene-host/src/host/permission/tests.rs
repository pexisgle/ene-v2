use super::{decision::Decisions, gate::Approved, *};
use crate::host::{
    Credentials, HostError, HostRuntime,
    client::{CapabilityApproval, CapabilitySpec},
    db::Store,
    mind::{Assignment, Assignments, Companion, Profile, State},
    notify::Notifier,
    resources::{DeviceHub, DeviceTimeout, Waiting},
};
use bevy_ecs::prelude::*;
use ene_mind::JsonDoc;
use ene_rpc as w;
use rig_core::catalog::{Catalog, Connector, ModelSpec};
use rig_core::completion::Message;
use rig_core::conversation::Conversation as History;
use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
use rig_core::providers::registry::ProviderId;
use rig_ecs::prelude::*;
use rig_ecs::session_log::SessionStore;
use rig_memory::DirStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const USER_REQUEST: &str = "デスクトップのスクリーンショットを取って";
/// A stall no test waits out: on the paused clock it only has to exceed
/// [`DECISION_TIMEOUT`].
const STALL: Duration = Duration::from_secs(600);

// === One gated call on a real world ==================================

/// A local OpenAI-compatible endpoint that answers every completion with
/// `text` after `delay`, keeping the request bodies it saw. The stalled
/// variants answer nothing a test waits for (compaction.rs's `serve`).
struct Provider {
    base: String,
    bodies: Arc<parking_lot::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(text: &'static str, delay: Duration) -> Provider {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    let answer = completion(text);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let body = loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    break None;
                };
                if read == 0 {
                    break None;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buffer[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break Some(String::from_utf8_lossy(&buffer[end + 4..]).to_string());
                }
            };
            let Some(body) = body else {
                return;
            };
            seen.lock().push(body);
            tokio::time::sleep(delay).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                answer.len()
            );
            let _discarded = socket.write_all(head.as_bytes()).await;
            let _discarded = socket.write_all(answer.as_bytes()).await;
            let _discarded = socket.flush().await;
        }
    });
    Provider { base, bodies, task }
}

fn completion(text: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-decision",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
    .to_string()
}

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // The timeout test fires DECISION_TIMEOUT without waiting it out.
        .start_paused(true)
        .build()
        .unwrap()
}

/// Advances the world and the paused runtime until every decision lands.
/// The parked sleep lets the runtime advance the clock to whichever
/// timer is due: the model reply or the decision timeout.
fn settle(app: &mut App, runtime: &tokio::runtime::Runtime) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut free = 0;
    while std::time::Instant::now() < deadline {
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        app.update();
        if app.world().resource::<Decisions>().running.is_empty() {
            free += 1;
            if free >= 2 {
                return;
            }
        } else {
            free = 0;
        }
    }
    panic!("the decision did not settle in time");
}

fn companion(assignments: Assignments) -> Companion {
    let now = ene_host_core::util::now();
    Companion {
        id: "ene".into(),
        name: "ene".into(),
        mind: "kind".into(),
        profile: Profile::default(),
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::empty(),
        voice: JsonDoc::empty(),
        assignments,
        persona: None,
        voice_identifier: None,
        revision: 1,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// The Decision role assigned, so the gate asks the model at `provider`.
fn decision_companion() -> Companion {
    let assignment = Assignment {
        connection_id: "c1".into(),
        model_id: "gpt-4o-mini".into(),
    };
    companion(Assignments {
        task: assignment.clone(),
        decision: Some(assignment),
        ..Default::default()
    })
}

/// Only the task model assigned: the Decision role falls back to it.
fn task_only_companion() -> Companion {
    companion(Assignments {
        task: Assignment {
            connection_id: "c1".into(),
            model_id: "gpt-4o-mini".into(),
        },
        ..Default::default()
    })
}

/// The catalog the decision connects through (compaction.rs's `models`).
fn models() -> Models {
    let provider = ProviderId::catalog("openrouter").unwrap();
    let mut catalog = Catalog::builtin().clone();
    catalog.insert(ModelSpec::new(provider, "gpt-4o-mini").with_context_window(128_000));
    Models(Connector::new(catalog))
}

/// The settings row the decision connects through: an OpenRouter adapter
/// aimed at `endpoint`.
fn connection_row(endpoint: &str) -> crate::host::settings::Connection {
    use crate::host::settings::{
        ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
    };
    crate::host::settings::Connection {
        id: "c1".into(),
        name: "main".into(),
        enabled: true,
        kind: ConnectionKind::Llm,
        config: ConnectionConfig::Llm(LlmConnection {
            endpoint: Endpoint {
                adapter: "openrouter".into(),
                endpoint: endpoint.into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            model_ids: vec!["gpt-4o-mini".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    }
}

fn files_spec() -> CapabilitySpec {
    CapabilitySpec {
        name: "device.files".into(),
        description: "files capability".into(),
        input_schema:
            r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}"#
                .into(),
        approval: CapabilityApproval::Confirm,
        approval_target: "{path}".into(),
        ..Default::default()
    }
}

/// One confirm-gated `device.files` client, activated for the companion.
/// The gate reads clients from the store, so the world needs no client
/// plugin machinery.
fn register_files_client(store: &Store) {
    let capability = "device.files".to_owned();
    store
        .client_repo()
        .register_client_with_grants(
            &crate::host::db::clients::ClientDeclaration {
                id: "c1",
                name: "c1",
                platform: "linux",
                capabilities: &[capability],
                specs: std::slice::from_ref(&files_spec()),
                expected_revision: 0,
            },
            &["device.files".to_owned()],
        )
        .unwrap();
    store.client_repo().activate("ene", Some("c1")).unwrap();
}

/// A headless world around one gated `device_request`: the store, a
/// client with a confirm-gated `device.files` capability, a companion
/// whose Decision model answers at `provider`, and the gate and collect
/// systems. The runtime is the caller's (paused, so a stall costs
/// nothing); the store lives in `_root`.
struct DecisionWorld {
    app: App,
    agent: Entity,
    _root: tempfile::TempDir,
}

impl DecisionWorld {
    fn at(runtime: &tokio::runtime::Runtime, provider: &str, owner: Companion) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        store
            .settings_repo()
            .update_settings(
                store.settings_repo().settings_revision().unwrap(),
                &[(connection_row(provider), 0)],
            )
            .unwrap();
        store.companion_repo().insert_companion(&owner).unwrap();
        register_files_client(&store);
        let credentials = Credentials::new(true);
        credentials.remember("c1", "test-key".into());
        // The kernel's start system runs on `Res<SessionLog>`, which the
        // restore of a session log inserts: the gate never runs without
        // it, so the world carries the plugins a real start does.
        let logs = DirStore::open(root.path().join("rig")).unwrap();
        let mut app = App::new();
        app.set_error_handler(bevy_ecs::error::warn)
            .insert_resource(store)
            .insert_resource(credentials)
            .insert_resource(SessionStore::new(logs))
            .insert_resource(Notifier::new())
            .insert_resource(Approvals::default())
            .init_resource::<DeviceHub>()
            .init_resource::<Waiting>()
            .insert_resource(DeviceTimeout(Duration::from_secs(30)))
            .insert_resource(models())
            .insert_resource(crate::host::HostState::default())
            .insert_resource(HostRuntime(runtime.handle().clone()))
            // The gate only sees a call the kernel has a handler for: a
            // `device_request` with no registered tool is dropped before
            // the gate runs, so the client plugin that registers it is
            // part of this world.
            .add_plugins((
                AgentPlugin,
                SessionLogPlugin,
                crate::host::client::ClientPlugin,
                GatePlugin,
            ));
        app.finish();
        if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
            time.set_max_delta(Duration::from_secs(60));
        }
        let mut history = History::new();
        history.push(Message::user(USER_REQUEST));
        // `Agent` and `AgentId` are the kernel's own identity for an
        // agent: without them the start stage finds no agent for the call
        // and drops it, so the gate never runs.
        let agent = app
            .world_mut()
            .spawn((
                Agent,
                AgentId(crate::host::agent::companion_agent_id("ene")),
                crate::host::agent::CompanionAgent {
                    companion_id: "ene".into(),
                },
                Conversation(history),
            ))
            .id();
        Self {
            app,
            agent,
            _root: root,
        }
    }

    fn store(&self) -> Store {
        self.app.world().resource::<Store>().clone()
    }

    /// Spawns one gated `device_request` call, the way a model reply does.
    fn gated_call(&mut self, path: &str) -> Entity {
        let call = ToolCall {
            id: CallId::from_wire("call-1"),
            function: ToolFunction::new(
                ToolName::new("device_request").unwrap(),
                serde_json::json!({"capability": "device.files", "input": {"path": path}}),
            ),
            native: None,
        };
        self.app
            .world_mut()
            .spawn((
                rig_ecs::tools::ToolRequest { call },
                rig_ecs::stage::Effect::default(),
                rig_ecs::stage::ForAgent(self.agent),
                rig_ecs::stage::Admitted,
            ))
            .id()
    }

    /// The one pending approval, once the gate has held the call.
    fn pending(&self) -> Approval {
        self.store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .pop()
            .expect("one held approval")
    }

    fn events(&mut self) -> Vec<w::Notification> {
        let mut receiver = self.app.world().resource::<Notifier>().subscribe();
        self.app.world_mut().resource_mut::<Notifier>().flush();
        let mut seen = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            seen.push(event);
        }
        seen
    }
}

/// What one notification stream says about cards and resolutions.
fn cards(events: &[w::Notification]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            let w::notification::Event::ApprovalRequested(request) = event.event.as_ref()? else {
                return None;
            };
            request
                .request
                .as_option()
                .map(|request| request.request_id.clone())
        })
        .collect()
}

fn notices(events: &[w::Notification], code: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event.as_ref(),
                Some(w::notification::Event::Generic(inner))
                    if inner.kind == "notice"
                        && inner.payload.as_option().is_some_and(|payload| {
                            payload.json.contains(code)
                        })
            )
        })
        .count()
}

#[test]
fn a_safe_operation_the_model_allows_runs_without_a_card() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"依頼どおりの安全な操作です"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    assert!(
        world.app.world().resource::<Decisions>().running.len() == 1,
        "the gate started one decision"
    );
    settle(&mut world.app, &runtime);

    let events = world.events();
    assert!(cards(&events).is_empty(), "no card: {events:?}");
    assert_eq!(notices(&events, "approval_resolved"), 1);
    let approval = world
        .store()
        .approval_repo()
        .approval(&request)
        .unwrap()
        .unwrap();
    assert_eq!(approval.status, ApprovalStatus::Approved);
    assert!(world.app.world().get::<Approved>(call).is_some());
    assert!(
        world
            .app
            .world()
            .resource::<Approvals>()
            .by_request
            .is_empty(),
        "the hold is released"
    );
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .len(),
        0
    );
    let bodies = provider.bodies.lock().clone();
    assert_eq!(bodies.len(), 1, "one decision call");
    provider.task.abort();
}

#[test]
fn an_operation_that_breaks_a_free_text_policy_is_carded_once() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"ポリシー: 外部への送信は常に確認"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    world
        .store()
        .policy_repo()
        .apply_policy_texts(
            &[PolicyText {
                id: "p1".into(),
                text: "外部への送信は常に確認する".into(),
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    settle(&mut world.app, &runtime);

    let events = world.events();
    assert_eq!(cards(&events), vec![request.clone()], "exactly one card");
    assert_eq!(notices(&events, "approval_required"), 1);
    let approval = world
        .store()
        .approval_repo()
        .approval(&request)
        .unwrap()
        .unwrap();
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert!(world.app.world().get::<Approved>(call).is_none());
    assert_eq!(
        world.app.world().resource::<Approvals>().by_request.len(),
        1,
        "the call stays held"
    );
    // The card the user answers is the real one: submission works.
    let resolved = submit(world.app.world_mut(), &request, ApprovalDecision::Deny).unwrap();
    assert_eq!(resolved.status, ApprovalStatus::Denied);
    provider.task.abort();
}

#[test]
fn a_timeout_or_an_invalid_reply_fails_safe_to_the_card() {
    let runtime = paused_runtime();
    // The endpoint stalls past the deadline: only the timeout answers.
    let provider = runtime.block_on(serve(r#"{"decision":"allow","reason":"遅すぎる"}"#, STALL));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    settle(&mut world.app, &runtime);
    let events = world.events();
    assert_eq!(cards(&events), vec![request.clone()], "the timeout cards");
    assert!(
        world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap()
            .status
            == ApprovalStatus::Pending
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    assert!(provider.bodies.lock().len() <= 1);
    provider.task.abort();

    // The reply is prose, not the JSON asked for.
    let provider = runtime.block_on(serve("はい、承認します", Duration::ZERO));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    settle(&mut world.app, &runtime);
    let events = world.events();
    assert_eq!(
        cards(&events),
        vec![request.clone()],
        "the invalid reply cards"
    );
    assert!(
        world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap()
            .status
            == ApprovalStatus::Pending
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    provider.task.abort();
}

#[test]
fn the_prompt_the_model_received_carries_the_bounded_inputs() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"対象が大きすぎる"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    world
        .store()
        .policy_repo()
        .apply_policy_texts(
            &[PolicyText {
                id: "p1".into(),
                text: "外部への送信は常に確認する".into(),
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    // A rule for another tool reaches the prompt as reference, without
    // deciding the call.
    world
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[PolicyRule {
                id: "r1".into(),
                tool_name: "body_motion".into(),
                pattern: "*".into(),
                effect: PolicyEffect::Allow,
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    let _discarded = world.gated_call("/tmp/a");
    world.app.update();
    settle(&mut world.app, &runtime);

    let bodies = provider.bodies.lock().clone();
    assert_eq!(bodies.len(), 1, "one decision call");
    let body = &bodies[0];
    assert!(body.contains("device_request"), "{body}");
    assert!(body.contains(USER_REQUEST), "{body}");
    assert!(body.contains("外部への送信は常に確認する"), "{body}");
    assert!(body.contains("body_motion / * → allow"), "{body}");
    // The canonical arguments reach the prompt; the body embeds it as a
    // JSON string, so its quotes travel escaped.
    let arguments = r#"{"capability":"device.files","input":{"path":"/tmp/a"}}"#;
    assert!(
        body.contains(&arguments.replace('"', "\\\"")),
        "the canonical arguments: {body}"
    );
    assert!(
        body.contains("require_approval"),
        "the system prompt demands the JSON shape: {body}"
    );
    provider.task.abort();
}

#[test]
fn a_late_decision_drops_when_the_user_answered_first() {
    let runtime = paused_runtime();
    // The model allows, but only after the user already denied.
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"安全だった"}"#,
        Duration::from_secs(1),
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    let denied = submit(world.app.world_mut(), &request, ApprovalDecision::Deny).unwrap();
    assert_eq!(denied.status, ApprovalStatus::Denied);
    settle(&mut world.app, &runtime);

    let events = world.events();
    assert!(cards(&events).is_empty(), "a late allow shows no card");
    assert!(
        world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap()
            .status
            == ApprovalStatus::Denied,
        "the deny stands"
    );
    assert!(
        world
            .store()
            .approval_repo()
            .resolve_approval(&request, ApprovalStatus::Approved)
            .is_err(),
        "the approval is resolved once"
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    assert!(world.app.world().resource::<Decisions>().running.is_empty());
    provider.task.abort();
}

#[test]
fn the_decision_role_falls_back_to_the_task_model() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"依頼どおりの操作"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, task_only_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    settle(&mut world.app, &runtime);

    assert_eq!(provider.bodies.lock().len(), 1, "the task model answered");
    let approval = world
        .store()
        .approval_repo()
        .approval(&request)
        .unwrap()
        .unwrap();
    assert_eq!(approval.status, ApprovalStatus::Approved);
    assert!(world.app.world().get::<Approved>(call).is_some());
    provider.task.abort();
}

#[test]
fn always_approve_persists_allow_rule_and_skips_second_prompt() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"外部ファイル操作"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call1 = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    let approved = submit(
        world.app.world_mut(),
        &request,
        ApprovalDecision::AlwaysApprove,
    )
    .unwrap();
    assert_eq!(approved.status, ApprovalStatus::Approved);
    assert!(world.app.world().get::<Approved>(call1).is_some());

    let rules = world.store().policy_repo().policy_rules().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].effect, PolicyEffect::Allow);

    // A second matching call is auto-approved directly from the saved rule
    // without asking the decision model or raising a second approval card.
    let calls_before = provider.bodies.lock().len();
    let call2 = world.gated_call("/tmp/a");
    world.app.update();
    assert!(
        world
            .app
            .world()
            .resource::<Waiting>()
            .by_operation
            .values()
            .any(|wait| wait.call == call2)
    );
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap(),
        [] as [ene_policy::Approval; 0]
    );
    assert_eq!(provider.bodies.lock().len(), calls_before);
    world.gated_call("/tmp/b");
    world.app.update();
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .len(),
        1
    );
    provider.task.abort();
}

#[test]
fn b20_confirm_rule_is_never_bypassed_by_decision_allow() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"safe"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    world
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[PolicyRule {
                id: "confirm-x".into(),
                tool_name: "device.files".into(),
                pattern: "/tmp/x".into(),
                effect: PolicyEffect::Confirm,
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    let call = world.gated_call("/tmp/x");
    world.app.update();
    assert!(
        world.app.world().resource::<Decisions>().running.is_empty(),
        "HumanRequired must skip Decision LLM"
    );
    let request = world.pending().request_id;
    let events = world.events();
    assert_eq!(cards(&events), vec![request.clone()]);
    assert!(world.app.world().get::<Approved>(call).is_none());
    assert_eq!(provider.bodies.lock().len(), 0);
    let resolved = submit(world.app.world_mut(), &request, ApprovalDecision::Approve).unwrap();
    assert_eq!(resolved.status, ApprovalStatus::Approved);
    assert!(world.app.world().get::<Approved>(call).is_some());
    provider.task.abort();
}

#[test]
fn b06_submit_keeps_hold_when_bound_client_loses_grant() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"ask"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    let mut client = world.store().client_repo().client("c1").unwrap().unwrap();
    client.granted_capabilities.clear();
    world.store().client_repo().restore_client(&client).unwrap();

    let err = submit(world.app.world_mut(), &request, ApprovalDecision::Approve).unwrap_err();
    assert!(matches!(err, HostError::Conflict(_)));
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap()
            .status,
        ApprovalStatus::Pending
    );
    assert!(
        world
            .app
            .world()
            .resource::<Approvals>()
            .by_request
            .contains_key(&request),
        "hold remains intact after pre-commit failure"
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    provider.task.abort();
}

#[test]
fn b18_large_always_approve_persists_exact_digest_rule_and_skips_second_prompt() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"large payload"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    let big_path = format!("/tmp/{}", "a".repeat(4200));
    let call1 = world.gated_call(&big_path);
    world.app.update();
    let request = world.pending().request_id;
    let outcome = submit_with_outcome(
        world.app.world_mut(),
        &request,
        ApprovalDecision::AlwaysApprove,
    )
    .unwrap();
    assert!(outcome.rule_saved, "{:?}", outcome.rule_error);
    assert!(world.app.world().get::<Approved>(call1).is_some());

    let rules = world.store().policy_repo().policy_rules().unwrap();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].pattern.starts_with("exact_sha256:"));

    let call2 = world.gated_call(&big_path);
    world.app.update();
    assert!(
        world
            .app
            .world()
            .resource::<Waiting>()
            .by_operation
            .values()
            .any(|wait| wait.call == call2)
    );
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap(),
        [] as [ene_policy::Approval; 0]
    );
    provider.task.abort();
}

#[test]
fn b05_policy_read_failure_fails_closed_instead_of_treating_as_empty() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"safe"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
    // Corrupt policy_rules row triggers HostError::Storage on policy_rules().
    world
        .store()
        .conn()
        .execute(
            "INSERT INTO policy_rules (id, tool_name, pattern, effect, created_at) \
                 VALUES ('bad', '*', '*', 'bogus', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    let call = world.gated_call("/tmp/a");
    world.app.update();
    assert!(
        world
            .app
            .world()
            .resource::<crate::host::HostState>()
            .failed
            .is_some(),
        "corrupted policy rules must fail the host instead of returning empty rules"
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    provider.task.abort();
}
