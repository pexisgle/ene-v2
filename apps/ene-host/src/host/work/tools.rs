//! Parent-facing work tools and the worker's report tool.
use super::{
    MAX_INSTRUCTION, WorkStatus, list,
    rig::{current_run, deliver_to_parent, notify, notify_run, parent_key, start_in_world},
};
use crate::host::{
    agent::AgentKey,
    chat,
    db::{Result, Store, now},
    error::HostError,
    tools,
};
use bevy_ecs::prelude::*;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    instruction: String,
    #[serde(default)]
    work_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    work_id: String,
    text: String,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkArgs {
    work_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    kind: String,
    content: String,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub(crate) fn register_tools(app: &mut App) {
    app.add_open_tool::<StartArgs, _>(
        "start_work",
        "Start independent work and return its work id immediately.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| start_tool(world, agent, args)),
    )
    .add_open_tool::<SendArgs, _>(
        "send_work",
        "Send more instructions to an existing work session.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| send_tool(world, agent, args)),
    )
    .add_open_tool::<WorkArgs, _>(
        "stop_work",
        "Stop a work session. The parent conversation keeps running.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args: WorkArgs| {
            stop_tool(world, agent, &args.work_id, args.reason.as_deref())
        }),
    )
    .add_open_tool::<Empty, _>(
        "list_works",
        "List this companion's work sessions.",
        ToolOptions {
            footprint: Some(Touches::Reads(Scope::Anything)),
        },
        tools::run_tool(|world, agent, _, _: Empty| list_works_tool(world, agent)),
    )
    .add_open_tool::<ReportArgs, _>(
        "report",
        "Report progress or a result to the parent companion.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args: ReportArgs| report_tool(world, agent, args)),
    );
}

fn start_tool(world: &mut World, agent: Entity, args: StartArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    if let Some(work_id) = args.work_id {
        return continue_work(world, &key, &work_id, args.instruction, None);
    }
    Ok(start_in_world(world, key.companion(), "", &args.instruction)?.id)
}

fn send_tool(world: &mut World, agent: Entity, args: SendArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    continue_work(world, &key, &args.work_id, args.text, args.request_id)
}

fn continue_work(
    world: &mut World,
    key: &AgentKey,
    work_id: &str,
    text: String,
    request_id: Option<String>,
) -> Result<String> {
    if text.trim().is_empty() || text.len() > MAX_INSTRUCTION {
        return Err(HostError::invalid("invalid work instruction"));
    }
    let store = world.resource::<Store>().clone();
    let mut session = store
        .work(work_id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if session.companion_id != key.companion() || session.closed {
        return Err(HostError::not_found("work not found"));
    }
    let child = chat::find_agent(world, &AgentKey::work(&session.companion_id, work_id))
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if let Some(inquiry) = request_id.as_deref() {
        store.answer_inquiry(work_id, inquiry)?;
    }
    world.trigger(
        Deliver::new(child, text, DeliveryMode::Steer).with_author(Author::agent(
            key.agent_id(),
            Some(RequestId(request_id.unwrap_or_else(|| work_id.to_owned()))),
        )),
    );
    session.status = WorkStatus::Active;
    session.updated_at = now();
    store.save_work(&session)?;
    notify(world, &session, "run_started", json!({}));
    Ok(work_id.to_owned())
}

pub(crate) fn stop_tool(
    world: &mut World,
    agent: Entity,
    id: &str,
    reason: Option<&str>,
) -> Result<String> {
    let key = parent_key(world, agent)?;
    let store = world.resource::<Store>().clone();
    let session = store
        .work(id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if session.companion_id != key.companion() {
        return Err(HostError::not_found("work not found"));
    }
    if session.closed {
        return Ok(id.into());
    }
    if let Some(child) = chat::find_agent(world, &AgentKey::work(&session.companion_id, id)) {
        world.trigger(StopTurn {
            entity: child,
            reason: Some(reason.unwrap_or("parent stopped work").into()),
        });
    }
    let mut session = session;
    session.status = WorkStatus::Idle;
    session.updated_at = now();
    world.resource::<Store>().save_work(&session)?;
    notify(world, &session, "status_changed", json!({"status": "idle"}));
    Ok(id.into())
}

fn list_works_tool(world: &mut World, agent: Entity) -> Result<String> {
    let key = parent_key(world, agent)?;
    let items: Vec<Value> = list(world, key.companion())?
        .into_iter()
        .map(|view| serde_json::to_value(view).unwrap_or(Value::Null))
        .collect();
    Ok(Value::Array(items).to_string())
}

fn report_tool(world: &mut World, agent: Entity, args: ReportArgs) -> Result<String> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    let Some(work) = key.work_id() else {
        return Err(HostError::permission("only work agents report"));
    };
    if args.content.trim().is_empty() || args.content.len() > MAX_INSTRUCTION {
        return Err(HostError::invalid("invalid report"));
    }
    if !matches!(
        args.kind.as_str(),
        "activity" | "message" | "question" | "completed" | "failed"
    ) {
        return Err(HostError::invalid("invalid report kind"));
    }
    let mut text = args.content;
    if !args.artifacts.is_empty() {
        text.push_str("\n\n添付:");
        for artifact in &args.artifacts {
            text.push_str("\n- ");
            text.push_str(artifact);
        }
    }
    if args.kind == "question" {
        let request_id = Uuid::new_v4().to_string();
        world
            .resource::<Store>()
            .add_inquiry(work, &request_id, &text, &[])?;
        deliver_to_parent(
            world,
            key.companion(),
            work,
            &format!("作業からの質問 (request_id: {request_id}):\n{text}"),
            Some(&format!("inquiry:{request_id}")),
        );
        let run = current_run(world, agent, work);
        notify_run(
            world,
            key.companion(),
            work,
            &run,
            "inquiry_created",
            json!({"request_id": request_id, "text": text}),
        );
        return Ok(request_id);
    }
    deliver_to_parent(world, key.companion(), work, &text, None);
    Ok("reported".into())
}
