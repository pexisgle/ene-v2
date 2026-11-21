//! Work tools: the parent's commission/stop tools and the hand's report.
use super::{WorkSession, WorkStatus};
use crate::host::{
    agent::{CompanionAgent, WorkAgent, WorkReports},
    db::{Result, Store},
    delegation,
    error::HostError,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartArgs {
    pub(crate) instruction: String,
    /// Continue this session instead of starting a new one.
    #[serde(default)]
    pub(crate) work_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    pub(crate) work_id: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StopArgs {
    pub(crate) work_id: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportArgs {
    /// activity | message | question | completed | failed
    pub(crate) kind: String,
    #[serde(alias = "summary", alias = "text", alias = "message")]
    pub(crate) content: String,
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<StartArgs, _>(
        "start_work",
        "Commission a task to your hands. Pass work_id to continue an existing session.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| start(world, agent, args)),
    )
    .add_open_tool::<SendArgs, _>(
        "send_work",
        "Send more input or the user's answer to a work session (running or paused on a question).",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| send(world, agent, args)),
    )
    .add_open_tool::<StopArgs, _>(
        "stop_work",
        "Ask a running task to stop.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| stop(world, agent, args)),
    )
    .add_open_tool::<ReportArgs, _>(
        "report",
        "Report progress (`activity`), important update (`message`), a clarifying question for the user (`question`), completion (`completed`), or failure (`failed`). The host binds the session and run.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| report(world, agent, args)),
    );
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))
}

fn own_session(world: &World, work_id: &str, companion: &str) -> Result<WorkSession> {
    let store = world.resource::<Store>();
    let candidate = work_id.strip_prefix("work-").unwrap_or(work_id);
    let session = match store.work_session(work_id)? {
        Some(session) => Some(session),
        None => match store.work_session(candidate)? {
            Some(session) => Some(session),
            None => match store.run(candidate)? {
                Some(run) => store.work_session(&run.work_id)?,
                None => None,
            },
        },
    }
    .ok_or_else(|| HostError::NotFound(("work session not found").into()))?;
    if session.companion_id != companion {
        return Err(HostError::Permission(
            ("this work session belongs to another companion").into(),
        ));
    }
    Ok(session)
}

fn start(world: &mut World, agent: Entity, args: StartArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    if args.instruction.trim().is_empty() {
        return Err(HostError::Invalid(("an instruction is required").into()));
    }
    // Every companion receives the same transcription. The delegation key is
    // the transcription id the host bound to this turn, never an LLM argument,
    // so only the first companion starts irreversible work (voice-avatar.md
    // §5.2). A manual chat message has no recorded transcription id and is
    // never claimed.
    if let Some(transcription_id) = transcription_id(world, agent)
        && !delegation::try_claim(world, &transcription_id, &companion)
    {
        // A peer already took the task: this is a normal result, not an error,
        // so the model can support the peer instead of retrying.
        return Ok(json!({
            "claimed": false,
            "reason": "another companion already took this task; support them or wait instead of starting duplicate work",
        })
        .to_string());
    }
    if let Some(work_id) = args.work_id.as_deref().filter(|id| !id.is_empty()) {
        let session = own_session(world, work_id, &companion)?;
        let resolved_id = session.id.clone();
        if session.status == WorkStatus::Closed {
            return Err(HostError::Conflict(("work session is closed").into()));
        }
        let store = world.resource::<Store>().clone();
        if store.active_run(&resolved_id)?.is_some() {
            return Err(HostError::Conflict(
                ("the work session is already running").into(),
            ));
        }
        let run = super::start_run(world, &session, &args.instruction)?;
        return Ok(json!({"work_id": resolved_id, "run_id": run.run_id}).to_string());
    }
    let session = super::start(
        world,
        w::StartWorkRequest {
            companion_id: companion,
            instruction: args.instruction,
            ..Default::default()
        },
    )?;
    Ok(json!({"work_id": session.id}).to_string())
}

/// The voice recognition event the current turn answers: the request id the
/// voice pipeline put on the latest user entry. A chat message carries a
/// request id there too, but only a transcription recorded by the voice
/// pipeline can be claimed, so manual chat is never treated as a delegation.
fn transcription_id(world: &World, agent: Entity) -> Option<String> {
    let conversation = world.get::<Conversation>(agent)?;
    conversation
        .entries()
        .rev()
        .find(|entry| entry.is_users_own())?
        .author?
        .request
        .as_ref()
        .map(|request| request.0.clone())
        .filter(|id| !id.is_empty())
}

fn send(world: &mut World, agent: Entity, args: SendArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let session = own_session(world, &args.work_id, &companion)?;
    let acceptance = super::send(
        world,
        w::SendWorkRequest {
            work_id: session.id,
            request_key: uuid::Uuid::new_v4().to_string(),
            text: args.text,
            ..Default::default()
        },
    )?;
    Ok(json!({"request_id": acceptance.request_id}).to_string())
}

fn stop(world: &mut World, agent: Entity, args: StopArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let session = own_session(world, &args.work_id, &companion)?;
    if session.status == WorkStatus::Closed {
        return Ok(json!({"work_id": session.id, "status": "closed"}).to_string());
    }
    super::stop_with(world, &session.id, args.reason.as_deref())?;
    Ok(json!({"work_id": session.id, "stopping": true}).to_string())
}

fn report(world: &mut World, agent: Entity, args: ReportArgs) -> Result<String> {
    let Some(work) = world.get::<WorkAgent>(agent).cloned() else {
        return Err(HostError::Permission(
            ("report is only available to a running task").into(),
        ));
    };
    let store = world.resource::<Store>().clone();
    // The active run is the source of truth, never the run captured when the
    // agent was spawned: a session agent serves several runs.
    let Some(run) = store.active_run(&work.work_id)? else {
        return Ok(
            json!({"accepted": false, "reason": "the session has no active run"}).to_string(),
        );
    };
    if run.state.is_terminal() || run.run_id != work.run_id {
        // A report from an older turn is audit data only; it must not touch
        // the run that is active now.
        return Ok(json!({"accepted": false, "reason": "the run already ended"}).to_string());
    }
    if args.content.trim().is_empty() {
        return Err(HostError::Invalid(("report content is required").into()));
    }
    let run_id = run.run_id.clone();
    let work_id = &work.work_id;
    match args.kind.as_str() {
        "activity" => {
            super::note_activity(world, &run_id);
            super::inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業報告 (work_id: {work_id})] {}", args.content),
                DeliveryMode::Inform,
            );
            Ok(json!({"accepted": true}).to_string())
        }
        "message" => {
            super::note_activity(world, &run_id);
            super::inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業連絡 (work_id: {work_id})] {}", args.content),
                DeliveryMode::Queue,
            );
            Ok(json!({"accepted": true}).to_string())
        }
        "question" => {
            super::note_activity(world, &run_id);
            world.resource_mut::<WorkReports>().completed.insert(
                run_id,
                format!("[作業質問 (work_id: {work_id})] {}", args.content),
            );
            world.trigger(StopTurn {
                entity: agent,
                reason: Some("the hand asked a question".into()),
            });
            Ok(json!({"accepted": true, "status": "waiting_for_answer"}).to_string())
        }
        "completed" => {
            super::note_activity(world, &run_id);
            world.resource_mut::<WorkReports>().completed.insert(
                run_id,
                format!("[作業完了 (work_id: {work_id})] {}", args.content),
            );
            Ok(json!({"accepted": true}).to_string())
        }
        "failed" => {
            // The settle path owns the run's terminal state and cleanup.
            world.resource_mut::<WorkReports>().failed.insert(
                run_id.clone(),
                format!("[作業失敗 (work_id: {work_id})] {}", args.content),
            );
            world.trigger(StopTurn {
                entity: agent,
                reason: Some("the hand reported a failure".into()),
            });
            super::inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業失敗 (work_id: {work_id})] {}", args.content),
                DeliveryMode::Queue,
            );
            Ok(json!({"accepted": true}).to_string())
        }
        other => Err(HostError::Invalid(format!("unknown report kind `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::delegation::DeliveredClaims;
    use crate::host::mind::{Assignments, Companion, Profile, State};
    use crate::host::model::{Credentials, ModelFactory};
    use crate::host::notify::Notifier;
    use crate::host::tests::bound;
    use crate::host::work::{RunState, Runs, new_run};
    use rig_core::completion::Message;
    use rig_core::conversation::{Entry, RequestId};
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use std::sync::Arc;

    #[test]
    fn report_binds_to_the_active_run_not_the_spawned_one() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        store
            .insert_work_session(&WorkSession {
                id: "w1".into(),
                companion_id: "ene".into(),
                title: "task".into(),
                status: WorkStatus::Active,
                client_id: None,
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap();
        store
            .insert_run(&new_run("run-b".into(), "w1", "do it", RunState::Running))
            .unwrap();
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Runs::default());
        world.insert_resource(WorkReports::default());
        let agent = world
            .spawn(WorkAgent {
                work_id: "w1".into(),
                run_id: "run-a".into(),
                companion_id: "ene".into(),
            })
            .id();
        let args = || ReportArgs {
            kind: "activity".into(),
            content: "working".into(),
        };

        assert!(
            super::report(&mut world, agent, args())
                .unwrap()
                .contains("\"accepted\":false")
        );
        world.entity_mut(agent).insert(WorkAgent {
            work_id: "w1".into(),
            run_id: "run-b".into(),
            companion_id: "ene".into(),
        });
        assert!(
            super::report(&mut world, agent, args())
                .unwrap()
                .contains("\"accepted\":true")
        );
    }

    fn companion(id: &str) -> Companion {
        Companion {
            id: id.into(),
            name: id.into(),
            mind: "core".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: crate::host::json::JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: crate::host::json::JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Assignments::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: crate::host::util::now(),
            updated_at: crate::host::util::now(),
        }
    }

    /// A world with just enough resources for `start_work` to create a run.
    fn work_world(store: Store) -> World {
        let model =
            MockCompletionModel::from_stream_turns(std::iter::empty::<Vec<MockStreamEvent>>());
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(DeliveredClaims::default());
        world.insert_resource(Runs::default());
        world.insert_resource(Notifier::new());
        world.insert_resource(Credentials::new(true));
        world.init_resource::<rig_ecs::model::Models>();
        world.insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
            Ok(bound(&model))
        })));
        world
    }

    /// A companion agent whose latest user entry answers `request`.
    fn companion_turn(world: &mut World, companion_id: &str, request: Option<&str>) -> Entity {
        let mut conversation = Conversation::default();
        conversation.push(Entry::new(Message::user("フォルダを調べて")).by(Author {
            request: request.map(|id| RequestId(id.to_owned())),
            ..Author::user()
        }));
        world
            .spawn((
                CompanionAgent {
                    companion_id: companion_id.into(),
                },
                conversation,
            ))
            .id()
    }

    #[test]
    fn only_the_first_companion_claims_a_voice_transcription() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        for id in ["ene", "kuro"] {
            store.insert_companion(&companion(id)).unwrap();
        }
        let mut world = work_world(store.clone());
        delegation::record(&mut world, "t1");
        let ene = companion_turn(&mut world, "ene", Some("t1"));
        let kuro = companion_turn(&mut world, "kuro", Some("t1"));
        let args = || StartArgs {
            instruction: "フォルダを調べて".into(),
            work_id: None,
        };

        let first = super::start(&mut world, ene, args()).unwrap();
        assert!(first.contains("\"work_id\""), "{first}");
        assert_eq!(store.work_sessions("ene").unwrap().len(), 1);

        // The peer gets an ordinary tool result, not an error, and no session.
        let second = super::start(&mut world, kuro, args()).unwrap();
        assert!(second.contains("\"claimed\":false"), "{second}");
        assert!(second.contains("another companion"), "{second}");
        assert!(store.work_sessions("kuro").unwrap().is_empty());
    }

    #[test]
    fn a_manual_chat_turn_is_not_a_delegation() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let mut world = work_world(store);
        let manual = companion_turn(&mut world, "ene", None);
        assert_eq!(transcription_id(&world, manual), None);

        // A chat acceptance request id is present in the conversation, but the
        // voice pipeline never recorded it, so it is not arbitrated.
        let chat = companion_turn(&mut world, "ene", Some("chat-request"));
        assert_eq!(
            transcription_id(&world, chat).as_deref(),
            Some("chat-request")
        );
        assert!(delegation::try_claim(&mut world, "chat-request", "kuro"));
    }
}
