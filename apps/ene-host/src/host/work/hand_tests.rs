//! The plugin hand on the real built reference guest: the whole flow a
//! companion's ACP task assignment names, from `start_work` to `stop_work`.

use super::hand;
use crate::host::{
    agent::{self, WorkAgent},
    db::Store,
    mind,
    mind::{ModelAssignment, ModelAssignments as MindAssignments},
    permission,
    permission::{ApprovalDecision, ApprovalStatus},
    providers::Providers,
    resources::HostRuntime,
    tests::{Harness, text_turn},
    work,
};
use ene_plugin::Extension;
use ene_rpc as w;
use rig_core::conversation::AuthorKind;
use rig_ecs::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The built reference guest, refusing to run a suite whose fixture is absent.
fn reference_wasm() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm");
    assert!(
        path.is_file(),
        "build the guest first: cargo build --release --locked -p ene-reference-wasm --target wasm32-unknown-unknown ({})",
        path.display()
    );
    path
}

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// The ACP connection a companion's task assignment names: no command, just
/// the adapter of the installed harness plugin.
fn acp_connection() -> crate::host::settings::Connection {
    use crate::host::settings::{
        AcpConnection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint,
    };
    crate::host::settings::Connection {
        id: "hand-acp".into(),
        name: "reference hand".into(),
        enabled: true,
        kind: ConnectionKind::Acp,
        config: ConnectionConfig::Acp(AcpConnection {
            endpoint: Endpoint {
                adapter: "reference".into(),
                endpoint: String::new(),
                options_schema: String::new(),
                options: String::new(),
            },
            command: String::new(),
            args: Vec::new(),
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    }
}

/// A host whose `reference` adapter is the installed harness plugin, plus a
/// companion whose task assignment names the ACP connection onto it.
struct HandWorld {
    harness: Harness,
    companion: String,
    /// Keeps the plugin package and the runtime the drivers run on.
    _root: TempDir,
    _runtime: tokio::runtime::Runtime,
}

impl HandWorld {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugins").join("reference");
        std::fs::create_dir_all(&plugin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for directory in [root.path().join("plugins"), plugin.clone()] {
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
        }
        write_private(
            &plugin.join("reference.wasm"),
            std::fs::read(reference_wasm()).unwrap(),
        );
        write_private(
            &plugin.join("manifest.json"),
            serde_json::json!({
                "contract_version": ene_plugin::CONTRACT_VERSION,
                "id": "reference", "name": "ene reference harness", "version": "1.0.0",
                "entrypoints": [{
                    "id": "harness", "target": "host", "file": "reference.wasm",
                    "provides": ["harness"], "uses": ["models", "tools"]
                }]
            })
            .to_string(),
        );
        let providers = Providers::load(root.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(providers.start("reference", Extension::Harness))
            .unwrap();
        // Enough turns for the head to answer every report the hand files.
        let mut harness = Harness::new((0..8).map(|_| text_turn("わかった")));
        harness.app.world_mut().insert_resource(providers.clone());
        harness
            .app
            .world_mut()
            .insert_resource(HostRuntime(runtime.handle().clone()));
        let store = harness.store();
        store
            .settings_repo()
            .update_settings(
                store.settings_repo().settings_revision().unwrap(),
                &[(acp_connection(), 0)],
            )
            .unwrap();
        let created = mind::create(
            harness.app.world_mut(),
            w::CreateCompanionRequest {
                name: "Mio".into(),
                mind: "kind".into(),
                assignments: Some(w::ModelAssignments::from(&MindAssignments {
                    task: ModelAssignment {
                        connection_id: "hand-acp".into(),
                        model_id: "reference".into(),
                    },
                    ..Default::default()
                }))
                .into(),
                ..Default::default()
            },
        )
        .unwrap();
        harness.app.update();
        Self {
            harness,
            companion: created.id,
            _root: root,
            _runtime: runtime,
        }
    }

    fn start_work(&mut self) -> w::WorkSession {
        work::start(
            self.harness.app.world_mut(),
            w::StartWorkRequest {
                companion_id: self.companion.clone(),
                title: "task".into(),
                instruction: "参照データを調べて".into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn send_work(&mut self, work_id: &str, text: &str) {
        work::send(
            self.harness.app.world_mut(),
            w::SendWorkRequest {
                work_id: work_id.into(),
                text: text.into(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// The texts the head's conversation holds from the hand, in order.
    fn reports(&mut self) -> Vec<String> {
        let companion = self.companion.clone();
        let world = self.harness.app.world_mut();
        let Some(parent) = agent::agent_of(world, &companion) else {
            return Vec::new();
        };
        world
            .get::<Conversation>(parent)
            .map_or_default(|conversation| {
                conversation
                    .entries()
                    .filter(|entry| {
                        entry
                            .author
                            .is_some_and(|author| author.kind == AuthorKind::Agent)
                    })
                    .filter_map(|entry| agent::entry_text(&entry))
                    .collect()
            })
    }

    fn pending(&self) -> permission::Approval {
        self.harness
            .store()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .first()
            .cloned()
            .expect("a pending approval")
    }
}

#[test]
fn an_acp_assignment_drives_the_plugin_hand_end_to_end() {
    let mut world = HandWorld::start();
    let session = world.start_work();
    assert!(
        hand::owns(
            world.harness.app.world().resource::<hand::Hands>(),
            &session.id
        ),
        "the session runs on the plugin hand"
    );

    // The turn parks on the plugin's permission request, which becomes an
    // approval row of the work session.
    world.harness.run_until(|frame| {
        frame
            .resource::<Store>()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .iter()
            .any(|approval| approval.work_id.as_deref() == Some(session.id.as_str()))
    });
    let request = world.pending();
    assert_eq!(request.work_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(
        request.capability.as_deref(),
        Some("reference.inspect"),
        "the card names the plugin's tool"
    );

    // Approving the card resumes the turn, which completes. The plugin's
    // progress reached the head as its reports: the head answers the
    // completion, and the reports the turn filed land with that answer.
    let run_id = request.run_id.clone();
    permission::submit(
        world.harness.app.world_mut(),
        &request.request_id,
        ApprovalDecision::Approve,
    )
    .unwrap();
    world.harness.run_until(|frame| {
        frame
            .resource::<Store>()
            .work_repo()
            .run(&run_id)
            .unwrap()
            .is_some_and(|run| run.state.is_terminal())
    });
    let run = world
        .harness
        .store()
        .work_repo()
        .run(&run_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, work::RunState::Completed);
    let companion = world.companion.clone();
    world
        .harness
        .run_until(move |frame| head_received(frame, &companion, "参照タスクを完了しました"));
    let reports = world.reports();
    assert!(
        reports.iter().any(|text| text.contains("[作業報告] 計画")),
        "the head received the plan: {reports:?}"
    );
    assert!(
        reports
            .iter()
            .any(|text| text.contains("参照データを確認します")),
        "the head received the progress message: {reports:?}"
    );
    assert!(
        reports
            .iter()
            .any(|text| text.contains("ツール reference.inspect")),
        "the head received the tool-call report: {reports:?}"
    );

    // A second session on the same connection is refused while this one is
    // open: one plugin hand per adapter at a time.
    let busy = work::start(
        world.harness.app.world_mut(),
        w::StartWorkRequest {
            companion_id: world.companion.clone(),
            instruction: "同じハーネスで別の作業".into(),
            ..Default::default()
        },
    );
    let error = busy.expect_err("the busy adapter refuses a second session");
    assert!(
        error
            .to_string()
            .contains("the harness plugin `reference` is already serving"),
        "the error names the busy harness: {error}"
    );

    // `send_work` drives a `Prompt` follow-up turn on the live session.
    world.send_work(&session.id, "続けて結果をまとめて");
    let companion = world.companion.clone();
    world
        .harness
        .run_until(move |frame| head_received(frame, &companion, "追加指示"));
    let runs = world.harness.store().work_repo().runs(&session.id).unwrap();
    assert!(
        runs.iter()
            .all(|run| run.state == work::RunState::Completed),
        "both turns completed: {runs:?}"
    );
}

/// Whether the head's conversation already holds a report containing `needle`.
fn head_received(frame: &mut bevy_ecs::prelude::World, companion: &str, needle: &str) -> bool {
    let Some(parent) = agent::agent_of(frame, companion) else {
        return false;
    };
    frame
        .get::<Conversation>(parent)
        .is_some_and(|conversation| {
            conversation
                .entries()
                .any(|entry| agent::entry_text(&entry).is_some_and(|text| text.contains(needle)))
        })
}

#[test]
fn stop_work_cancels_a_parked_plugin_hand_run() {
    let mut world = HandWorld::start();
    let session = world.start_work();
    world.harness.run_until(|frame| {
        !frame
            .resource::<Store>()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    let run_id = world.pending().run_id.clone();

    // The stop is final at once, the way the rig hand's stop is.
    work::stop(world.harness.app.world_mut(), &session.id).unwrap();
    assert_eq!(
        world
            .harness
            .store()
            .work_repo()
            .run(&run_id)
            .unwrap()
            .unwrap()
            .state,
        work::RunState::Cancelled
    );

    // The guest answers its `Cancel` with `Stop{Cancelled}`; the settled run
    // ignores it, and the approval the turn parked on expires.
    world.harness.run_until(|frame| {
        frame
            .resource::<Store>()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    assert_eq!(
        world
            .harness
            .store()
            .work_repo()
            .run(&run_id)
            .unwrap()
            .unwrap()
            .state,
        work::RunState::Cancelled,
        "the late guest stop does not touch the settled run"
    );

    // The claim ends with the session, so a fresh session can start again.
    work::close(world.harness.app.world_mut(), &session.id).unwrap();
    let second = world.start_work();
    assert!(
        hand::owns(
            world.harness.app.world().resource::<hand::Hands>(),
            &second.id
        ),
        "a closed session releases the harness"
    );
    world.harness.run_until(|frame| {
        !frame
            .resource::<Store>()
            .approval_repo()
            .approvals(ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
}

#[test]
fn the_rig_hand_stays_the_default_without_an_acp_assignment() {
    let mut world = HandWorld::start();
    // The same host runs the reference harness, but this companion's task
    // connection is an LLM connection, so the backend stays in-process.
    let store = world.harness.store();
    let llm = crate::host::settings::Connection {
        id: "llm-main".into(),
        name: "main".into(),
        enabled: true,
        kind: crate::host::settings::ConnectionKind::Llm,
        config: crate::host::settings::ConnectionConfig::Llm(
            crate::host::settings::LlmConnection {
                endpoint: crate::host::settings::Endpoint {
                    adapter: "openai".into(),
                    ..Default::default()
                },
                model_ids: vec!["deepseek/deepseek-flash".into()],
            },
        ),
        credential_state: crate::host::settings::CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(llm, 0)],
        )
        .unwrap();
    let created = mind::create(
        world.harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "Kai".into(),
            mind: "calm".into(),
            assignments: Some(w::ModelAssignments::from(&MindAssignments {
                task: ModelAssignment {
                    connection_id: "llm-main".into(),
                    model_id: "deepseek/deepseek-flash".into(),
                },
                ..Default::default()
            }))
            .into(),
            ..Default::default()
        },
    )
    .unwrap();
    world.harness.app.update();
    let session = work::start(
        world.harness.app.world_mut(),
        w::StartWorkRequest {
            companion_id: created.id,
            instruction: "folder".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !hand::owns(
            world.harness.app.world().resource::<hand::Hands>(),
            &session.id
        ),
        "an LLM task connection keeps the rig backend"
    );
    let work_agent = {
        let frame = world.harness.app.world_mut();
        let mut query = frame.query::<&WorkAgent>();
        query.iter(frame).any(|agent| agent.work_id == session.id)
    };
    assert!(work_agent, "the rig hand spawned its in-process agent");
    world.harness.run_until(|frame| {
        frame
            .resource::<Store>()
            .work_repo()
            .runs(&session.id)
            .unwrap()
            .iter()
            .any(|run| run.state.is_terminal())
    });
    assert_eq!(
        world.harness.store().work_repo().runs(&session.id).unwrap()[0].state,
        work::RunState::Completed
    );
}

#[test]
fn an_acp_connection_needs_only_its_adapter() {
    use crate::host::settings;
    assert!(acp_connection().validate().is_ok());
    let legacy = settings::Connection {
        id: "legacy".into(),
        name: "legacy".into(),
        enabled: true,
        kind: settings::ConnectionKind::Acp,
        config: settings::ConnectionConfig::Acp(settings::AcpConnection {
            endpoint: settings::Endpoint {
                adapter: "some-agent".into(),
                ..Default::default()
            },
            command: "/usr/bin/some-agent".into(),
            args: vec!["--stdin".into()],
        }),
        credential_state: settings::CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    assert!(legacy.validate().is_ok(), "the legacy command stays valid");
    let empty = settings::Connection {
        id: "empty".into(),
        name: "empty".into(),
        enabled: true,
        kind: settings::ConnectionKind::Acp,
        config: settings::ConnectionConfig::Acp(settings::AcpConnection {
            endpoint: settings::Endpoint::default(),
            command: String::new(),
            args: Vec::new(),
        }),
        credential_state: settings::CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    assert!(
        empty.validate().is_err(),
        "the adapter is the one field an ACP connection requires"
    );
}
