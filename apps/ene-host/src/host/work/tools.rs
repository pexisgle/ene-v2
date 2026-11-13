//! Work tools: the parent's commission/steer/stop tools and the hand's
//! report tool.
use super::{Inquiry, Run, RunState, WorkStatus};
use crate::host::{
    agent::{self, CompanionAgent, WorkAgent, WorkReports},
    db::{Result, Store},
    error::HostError,
    notify::Notifier,
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

fn start(world: &mut World, agent: Entity, args: StartArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    if args.instruction.trim().is_empty() {
        return Err(HostError::invalid("an instruction is required"));
    }
    let store = world.resource::<Store>().clone();
    if let Some(work_id) = args.work_id.as_deref().filter(|id| !id.is_empty()) {
        let session = store
            .work_session(work_id)?
            .ok_or_else(|| HostError::not_found("work session not found"))?;
        if session.companion_id != companion {
            return Err(HostError::permission(
                "this work session belongs to another companion",
            ));
        }
        if session.status == WorkStatus::Closed {
            return Err(HostError::conflict("work session is closed"));
        }
        if store.active_run(work_id)?.is_some() {
            return Err(HostError::conflict("the work session is already running"));
        }
        let companion_record = store
            .companion(&companion)?
            .ok_or_else(|| HostError::not_found("companion not found"))?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let run = Run {
            run_id: run_id.clone(),
            work_id: work_id.to_owned(),
            instruction: args.instruction.clone(),
            state: RunState::Running,
            pending_request_id: None,
            failure: None,
            cancellation_requested_at: None,
            cancellation_reason: None,
            unverified_pids: Vec::new(),
            started_at: String::new(),
            updated_at: String::new(),
        };
        store.insert_run(&run)?;
        store.set_work_status(work_id, WorkStatus::Active)?;
        let work_agent = agent::spawn_work_agent(
            world,
            &companion_record,
            work_id,
            &run_id,
            &args.instruction,
        );
        agent::ensure_model(world, work_agent)?;
        world.trigger(Deliver::new(
            work_agent,
            args.instruction.clone(),
            DeliveryMode::Queue,
        ));
        return Ok(json!({"work_id": work_id, "run_id": run_id}).to_string());
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
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(&args.work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.companion_id != companion {
        return Err(HostError::permission(
            "this work session belongs to another companion",
        ));
    }
    let request_id = args.request_id.clone().unwrap_or_default();
    let acceptance = super::send(
        world,
        w::SendWorkRequest {
            work_id: args.work_id,
            request_key: uuid::Uuid::new_v4().to_string(),
            text: args.text,
            request_id: Some(request_id),
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
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(&args.work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.companion_id != companion {
        return Err(HostError::permission(
            "this work session belongs to another companion",
        ));
    }
    if session.status == WorkStatus::Closed {
        return Ok(json!({"work_id": session.id, "status": "closed"}).to_string());
    }
    super::stop(world, &args.work_id)?;
    let _ = args.reason;
    Ok(json!({"work_id": session.id, "stopping": true}).to_string())
}

fn report(world: &mut World, agent: Entity, args: ReportArgs) -> Result<String> {
    let Some(work) = world.get::<WorkAgent>(agent).cloned() else {
        return Err(HostError::permission(
            "report is only available to a running task",
        ));
    };
    let store = world.resource::<Store>().clone();
    let Some(run) = store.run(&work.run_id)? else {
        return Err(HostError::not_found("run not found"));
    };
    if run.state.is_terminal() {
        return Ok(json!({"accepted": false, "reason": "the run already ended"}).to_string());
    }
    if args.content.trim().is_empty() {
        return Err(HostError::invalid("report content is required"));
    }
    match args.kind.as_str() {
        "activity" | "message" => {
            if let Some(parent) = agent::agent_of(world, &work.companion_id) {
                let author = Author::agent(agent::work_agent_id(&work.run_id), None);
                world.trigger(
                    Deliver::new(
                        parent,
                        format!("[作業報告] {}", args.content),
                        DeliveryMode::Inform,
                    )
                    .with_author(author),
                );
            }
            Ok(json!({"accepted": true}).to_string())
        }
        "question" => {
            let request_id = uuid::Uuid::new_v4().to_string();
            store.insert_inquiry(&Inquiry {
                request_id: request_id.clone(),
                work_id: work.work_id.clone(),
                run_id: work.run_id.clone(),
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
            let event = crate::host::wire::work_event(
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
            let companion = session.companion_id.clone();
            {
                let mut notifier = world.resource_mut::<Notifier>();
                notifier.push(
                    Some(&companion),
                    None,
                    w::notification::Event::WorkEvent(Box::new(event)),
                );
            }
            if let Some(parent) = agent::agent_of(world, &companion) {
                let author = Author::agent(agent::work_agent_id(&work.run_id), None);
                world.trigger(
                    Deliver::new(
                        parent,
                        format!("[作業からの質問 request_id={request_id}] {}", args.content),
                        DeliveryMode::Queue,
                    )
                    .with_author(author),
                );
            }
            Ok(json!({"accepted": true, "request_id": request_id}).to_string())
        }
        "completed" => {
            world
                .resource_mut::<WorkReports>()
                .completed
                .insert(work.run_id.clone(), args.content.clone());
            Ok(json!({"accepted": true}).to_string())
        }
        "failed" => {
            let mut failed = run.clone();
            failed.state = RunState::Failed;
            failed.failure = Some(crate::host::error::Failure::new(
                "work_failed",
                args.content.clone(),
            ));
            store.update_run(&failed)?;
            if let Some(parent) = agent::agent_of(world, &work.companion_id) {
                let author = Author::agent(agent::work_agent_id(&work.run_id), None);
                world.trigger(
                    Deliver::new(
                        parent,
                        format!("[作業失敗] {}", args.content),
                        DeliveryMode::Queue,
                    )
                    .with_author(author),
                );
            }
            Ok(json!({"accepted": true}).to_string())
        }
        other => Err(HostError::invalid(format!("unknown report kind `{other}`"))),
    }
}
