//! Work tools: the parent's commission/steer/stop tools and the hand's report.
use super::{Inquiry, RunState, WorkSession, WorkStatus};
use crate::host::{
    agent::{self, CompanionAgent, WorkAgent, WorkReports},
    db::{Result, Store},
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
    /// The pending inquiry this text answers.
    #[serde(default)]
    pub(crate) request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListArgs {}

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
        "Send more input to a running task, or answer a pending question.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| send(world, agent, args)),
    )
    .add_open_tool::<ListArgs, _>(
        "list_works",
        "List the work sessions you own and their runs.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| list(world, agent, args)),
    )
    .add_open_tool::<StopArgs, _>(
        "stop_work",
        "Ask a running task to stop.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| stop(world, agent, args)),
    )
    .add_open_tool::<ReportArgs, _>(
        "report",
        "Report progress, ask the user a question, or report completion or failure. \
         The host binds the session and run.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| report(world, agent, args)),
    );
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::internal("this tool belongs to a companion"))
}

fn own_session(world: &World, work_id: &str, companion: &str) -> Result<WorkSession> {
    let session = super::require_session(world.resource::<Store>(), work_id)?;
    if session.companion_id != companion {
        return Err(HostError::permission(
            "this work session belongs to another companion",
        ));
    }
    Ok(session)
}

fn start(world: &mut World, agent: Entity, args: StartArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    if args.instruction.trim().is_empty() {
        return Err(HostError::invalid("an instruction is required"));
    }
    if let Some(work_id) = args.work_id.as_deref().filter(|id| !id.is_empty()) {
        let session = own_session(world, work_id, &companion)?;
        if session.status == WorkStatus::Closed {
            return Err(HostError::conflict("work session is closed"));
        }
        let store = world.resource::<Store>().clone();
        if store.active_run(work_id)?.is_some() {
            return Err(HostError::conflict("the work session is already running"));
        }
        let run = super::start_run(world, &session, &args.instruction)?;
        return Ok(json!({"work_id": work_id, "run_id": run.run_id}).to_string());
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

fn send(world: &mut World, agent: Entity, args: SendArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    own_session(world, &args.work_id, &companion)?;
    let acceptance = super::send(
        world,
        w::SendWorkRequest {
            work_id: args.work_id,
            request_key: uuid::Uuid::new_v4().to_string(),
            text: args.text,
            request_id: Some(args.request_id.unwrap_or_default()),
            ..Default::default()
        },
    )?;
    Ok(json!({"request_id": acceptance.request_id}).to_string())
}

fn list(world: &mut World, agent: Entity, _args: ListArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let mut sessions = Vec::new();
    for session in store.work_sessions(&companion)? {
        let runs: Vec<String> = store
            .runs(&session.id)?
            .iter()
            .map(|run| format!("{} ({})", run.run_id, run.state.as_str()))
            .collect();
        sessions.push(json!({
            "work_id": session.id,
            "title": session.title,
            "status": session.status.as_str(),
            "runs": runs,
        }));
    }
    Ok(json!({"sessions": sessions}).to_string())
}

fn stop(world: &mut World, agent: Entity, args: StopArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let session = own_session(world, &args.work_id, &companion)?;
    if session.status == WorkStatus::Closed {
        return Ok(json!({"work_id": session.id, "status": "closed"}).to_string());
    }
    super::stop_with(world, &args.work_id, args.reason.as_deref())?;
    Ok(json!({"work_id": session.id, "stopping": true}).to_string())
}

fn report(world: &mut World, agent: Entity, args: ReportArgs) -> Result<String> {
    let Some(work) = world.get::<WorkAgent>(agent).cloned() else {
        return Err(HostError::permission(
            "report is only available to a running task",
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
        return Err(HostError::invalid("report content is required"));
    }
    let run_id = run.run_id.clone();
    let cancelling = run.state == RunState::Cancelling;
    match args.kind.as_str() {
        "activity" | "message" => {
            if cancelling {
                return cancelled();
            }
            super::note_activity(world, &run_id);
            inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業報告] {}", args.content),
                DeliveryMode::Inform,
            );
            Ok(json!({"accepted": true}).to_string())
        }
        "question" => {
            if cancelling {
                return cancelled();
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            store.insert_inquiry(&Inquiry {
                request_id: request_id.clone(),
                work_id: work.work_id.clone(),
                run_id: run_id.clone(),
                prompt: args.content.clone(),
                choices: Vec::new(),
                answer: None,
                created_at: String::new(),
                resolved_at: None,
            })?;
            let mut waiting = run.clone();
            waiting.state = RunState::WaitingInput;
            waiting.pending_request_id = Some(request_id.clone());
            store.update_run(&waiting)?;
            let session = store
                .work_session(&work.work_id)?
                .ok_or_else(|| HostError::not_found("work session not found"))?;
            super::notify_event(
                world,
                &session,
                w::WorkEventKind::InquiryCreated,
                w::work_event::Detail::InquiryCreated(Box::new(w::WorkInquiryCreated {
                    inquiry: Some(w::WorkInquiry {
                        request_id: request_id.clone(),
                        prompt: args.content.clone(),
                        choices: Vec::new(),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                })),
            );
            inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業からの質問 request_id={request_id}] {}", args.content),
                DeliveryMode::Queue,
            );
            Ok(json!({"accepted": true, "request_id": request_id}).to_string())
        }
        "completed" => {
            if cancelling {
                return cancelled();
            }
            super::note_activity(world, &run_id);
            world
                .resource_mut::<WorkReports>()
                .completed
                .insert(run_id, args.content);
            Ok(json!({"accepted": true}).to_string())
        }
        "failed" => {
            // The settle path owns the run's terminal state and cleanup.
            world
                .resource_mut::<WorkReports>()
                .failed
                .insert(run_id.clone(), args.content.clone());
            world.trigger(StopTurn {
                entity: agent,
                reason: Some("the hand reported a failure".into()),
            });
            inform(
                world,
                &work.companion_id,
                &run_id,
                format!("[作業失敗] {}", args.content),
                DeliveryMode::Queue,
            );
            Ok(json!({"accepted": true}).to_string())
        }
        other => Err(HostError::invalid(format!("unknown report kind `{other}`"))),
    }
}

fn cancelled() -> Result<String> {
    Ok(json!({"accepted": false, "reason": "the run is cancelling"}).to_string())
}

fn inform(world: &mut World, companion: &str, run_id: &str, text: String, mode: DeliveryMode) {
    let Some(parent) = agent::agent_of(world, companion) else {
        return;
    };
    let author = Author::agent(agent::work_agent_id(run_id), None);
    world.trigger(Deliver::new(parent, text, mode).with_author(author));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::work::{Runs, new_run};

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
                activation_revision: 0,
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
}
