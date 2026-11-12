//! Independent work sessions, their runs and the parent report.
use super::{
    agent::AgentKey,
    chat::{self, TurnState},
    client,
    db::{Result, Store, digest, new_id, now, validate_key},
    error::HostError,
    notify::Notifier,
    tick, tools, wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const WORK_PROMPT: &str = "You are a work agent acting for a companion. Work from the supplied brief, report clearly, and do not expose the parent's private history.";
const MAX_INSTRUCTION: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkStatus {
    Idle,
    Active,
    Closed,
}

impl WorkStatus {
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "closed" => Self::Closed,
            _ => Self::Idle,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WorkSession {
    pub(crate) id: String,
    pub(crate) companion_id: String,
    pub(crate) title: String,
    pub(crate) status: WorkStatus,
    pub(crate) reported_run: Option<String>,
    pub(crate) closed: bool,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

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

pub(crate) fn start(
    app: &mut App,
    companion_id: String,
    title: String,
    instruction: String,
    client_id: String,
) -> Result<w::WorkSession> {
    if !client_id.is_empty() {
        client::activate_on_send(app.world_mut(), &companion_id, &client_id)?;
    }
    start_in_world(app.world_mut(), &companion_id, &title, &instruction)
}

fn start_in_world(
    world: &mut World,
    companion_id: &str,
    title: &str,
    instruction: &str,
) -> Result<w::WorkSession> {
    if instruction.trim().is_empty() || instruction.len() > MAX_INSTRUCTION {
        return Err(HostError::invalid("invalid work instruction"));
    }
    world
        .resource::<Store>()
        .companion(companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let id = new_id();
    let session = WorkSession {
        id: id.clone(),
        companion_id: companion_id.into(),
        title: if title.trim().is_empty() {
            crate::truncate_chars(instruction, 80)
        } else {
            crate::truncate_chars(title, 200)
        },
        status: WorkStatus::Active,
        reported_run: None,
        closed: false,
        created_at: now(),
        updated_at: now(),
    };
    world.resource::<Store>().save_work(&session)?;
    let key = AgentKey::work(companion_id, &id);
    let child = chat::spawn_agent(world, &key);
    world.entity_mut(child).insert(SystemPrompt(format!(
        "{WORK_PROMPT}\n\n作業: {instruction}"
    )));
    world.trigger(
        Deliver::new(child, instruction, DeliveryMode::Steer).with_author(Author::agent(
            AgentKey::parent(companion_id).agent_id(),
            Some(RequestId(id.clone())),
        )),
    );
    notify(
        world,
        &session,
        "status_changed",
        json!({"status": "active"}),
    );
    Ok(wire::work_session(&session))
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    for mut session in store.works()? {
        if session.status == WorkStatus::Active {
            session.status = WorkStatus::Idle;
            session.updated_at = now();
            store.save_work(&session)?;
        }
        if let Some(child) =
            chat::find_agent(world, &AgentKey::work(&session.companion_id, &session.id))
        {
            report_work(world, child)?;
        }
    }
    Ok(())
}

pub(crate) fn list(world: &mut World, companion_id: &str) -> Result<Vec<w::WorkView>> {
    let store = world.resource::<Store>().clone();
    let mut views = Vec::new();
    for session in store.works_for(companion_id)? {
        views.push(view(world, &session.id)?);
    }
    Ok(views)
}

pub(crate) fn view(world: &mut World, id: &str) -> Result<w::WorkView> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work(id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    let key = AgentKey::work(&session.companion_id, id);
    let runs = chat::find_agent(world, &key)
        .and_then(|child| world.get::<Conversation>(child))
        .map(|conversation| runs(conversation, id))
        .unwrap_or_default();
    Ok(w::WorkView {
        session: wire::work_session(&session).into(),
        runs,
        pending_inquiries: store.pending_inquiries(id)?,
        ..Default::default()
    })
}

fn runs(conversation: &Conversation, work_id: &str) -> Vec<w::RunInfo> {
    conversation
        .turns()
        .map(|(_, turn)| {
            let (state, failure) = chat::turn_state(turn.end.as_ref());
            w::RunInfo {
                run_id: format!("{work_id}:{}", turn.start.get()),
                work_id: work_id.into(),
                state: state.as_str().into(),
                failure: failure.as_ref().map(wire::failure).into(),
                ..Default::default()
            }
        })
        .collect()
}

pub(crate) fn send(app: &mut App, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    validate_key(&request.request_key)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_INSTRUCTION {
        return Err(HostError::invalid("invalid work instruction"));
    }
    let store = app.world().resource::<Store>().clone();
    let session = store
        .work(&request.work_id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if session.closed {
        return Err(HostError::conflict("work is closed"));
    }
    let key = AgentKey::work(&session.companion_id, &request.work_id);
    let hash = digest(
        format!(
            "{}:{}:{}",
            request.work_id,
            request.text,
            request.request_id.as_deref().unwrap_or_default()
        )
        .as_bytes(),
    );
    let child = chat::find_agent(app.world_mut(), &key)
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if let Some(accepted) = chat::accepted(app.world_mut(), child, &request.request_key) {
        if accepted.hash != hash {
            return Err(HostError::already_exists(
                "request key was used with different content",
            ));
        }
        if let Some(inquiry) = request.request_id {
            store.answer_inquiry(&request.work_id, &inquiry)?;
        }
        return Ok(w::WorkAcceptance {
            request_id: accepted.request_id,
            ..Default::default()
        });
    }
    let request_id = chat::deliver_accepted(app, child, &request.request_key, hash, request.text)?;
    if let Some(request_id) = request.request_id {
        store.answer_inquiry(&request.work_id, &request_id)?;
    }
    let mut session = session;
    session.status = WorkStatus::Active;
    session.updated_at = now();
    store.save_work(&session)?;
    notify(app.world_mut(), &session, "run_started", json!({}));
    Ok(w::WorkAcceptance {
        request_id,
        ..Default::default()
    })
}

pub(crate) fn answer_inquiry(
    app: &mut App,
    request: w::AnswerInquiryRequest,
) -> Result<w::WorkAcceptance> {
    let store = app.world().resource::<Store>().clone();
    let session = store
        .work(&request.work_id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if session.closed {
        return Err(HostError::conflict("work is closed"));
    }
    if !store
        .pending_inquiries(&request.work_id)?
        .iter()
        .any(|inquiry| inquiry.request_id == request.request_id)
    {
        return Err(HostError::not_found("inquiry not found"));
    }
    let key = AgentKey::work(&session.companion_id, &request.work_id);
    let child = chat::find_agent(app.world_mut(), &key)
        .ok_or_else(|| HostError::not_found("work not found"))?;
    app.world_mut().trigger(
        Deliver::new(child, request.answer, DeliveryMode::Steer).with_author(Author::agent(
            AgentKey::parent(&session.companion_id).agent_id(),
            Some(RequestId(request.request_id.clone())),
        )),
    );
    tick(app);
    store.answer_inquiry(&request.work_id, &request.request_id)?;
    Ok(w::WorkAcceptance {
        request_id: request.request_id,
        ..Default::default()
    })
}

pub(crate) fn stop(app: &mut App, id: &str) -> Result<()> {
    let store = app.world().resource::<Store>().clone();
    let mut session = store
        .work(id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    if session.closed {
        return Ok(());
    }
    if let Some(child) =
        chat::find_agent(app.world_mut(), &AgentKey::work(&session.companion_id, id))
    {
        app.world_mut().trigger(StopTurn {
            entity: child,
            reason: Some("user stopped work".into()),
        });
    }
    session.status = WorkStatus::Idle;
    session.updated_at = now();
    store.save_work(&session)?;
    notify(
        app.world_mut(),
        &session,
        "status_changed",
        json!({"status": "idle"}),
    );
    tick(app);
    Ok(())
}

pub(crate) fn close(app: &mut App, id: &str) -> Result<()> {
    stop(app, id)?;
    let store = app.world().resource::<Store>().clone();
    let mut session = store
        .work(id)?
        .ok_or_else(|| HostError::not_found("work not found"))?;
    session.closed = true;
    session.status = WorkStatus::Closed;
    session.updated_at = now();
    store.save_work(&session)?;
    notify(
        app.world_mut(),
        &session,
        "status_changed",
        json!({"status": "closed"}),
    );
    Ok(())
}

pub(crate) fn report_work(world: &mut World, worker: Entity) -> Result<()> {
    let Some(key) = world.get::<AgentKey>(worker).cloned() else {
        return Ok(());
    };
    let Some(work_id) = key.work_id() else {
        return Ok(());
    };
    let store = world.resource::<Store>().clone();
    let Some(mut session) = store.work(work_id)? else {
        return Ok(());
    };
    let Some(conversation) = world.get::<Conversation>(worker) else {
        return Ok(());
    };
    let Some((_, turn)) = conversation.turns().next_back() else {
        return Ok(());
    };
    let Some(end) = turn.end.as_ref() else {
        return Ok(());
    };
    let run = format!("{work_id}:{}", turn.start.get());
    if session.reported_run.as_deref() == Some(&run) {
        return Ok(());
    }
    let (state, failure) = chat::turn_state(Some(end));
    let text = chat::last_assistant(conversation, turn.start)
        .map(|(_, text)| text)
        .unwrap_or_default();
    let message = match failure {
        Some(failure) => format!(
            "作業 {} は {} で終了しました（{}）。\n{text}",
            work_id,
            state.as_str(),
            failure.message
        ),
        None => format!(
            "作業 {} は {} で終了しました。\n{text}",
            work_id,
            state.as_str()
        ),
    };
    let parent = chat::ensure_parent(world, &session.companion_id);
    let delivered = chat::has_request(world, parent, &format!("report:{run}"));
    let mut changed = session.status != WorkStatus::Idle;
    if changed {
        session.status = WorkStatus::Idle;
        session.updated_at = now();
    }
    if !delivered {
        deliver_to_parent(world, &session.companion_id, work_id, &message, Some(&run));
    } else if session.reported_run.as_deref() != Some(&run) {
        session.reported_run = Some(run.clone());
        session.updated_at = now();
        changed = true;
    }
    if changed {
        store.save_work(&session)?;
    }
    if changed {
        notify(
            world,
            &session,
            "run_finished",
            json!({"run_id": run, "state": state.as_str()}),
        );
    }
    Ok(())
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
        return Ok(request_id);
    }
    deliver_to_parent(world, key.companion(), work, &text, None);
    Ok("reported".into())
}

pub(crate) fn deliver_to_parent(
    world: &mut World,
    companion: &str,
    work: &str,
    text: &str,
    run: Option<&str>,
) {
    let parent = chat::ensure_parent(world, companion);
    let request_id = run.map(|run| format!("report:{run}"));
    if request_id
        .as_deref()
        .is_some_and(|id| chat::has_request(world, parent, id))
    {
        return;
    }
    world.trigger(
        Deliver::new(parent, text, DeliveryMode::Queue)
            .with_author(Author::agent(work, request_id.map(RequestId))),
    );
}

pub(crate) fn notify(world: &mut World, session: &WorkSession, kind: &str, detail: Value) {
    world.resource_mut::<Notifier>().push(
        Some(session.companion_id.clone()),
        None,
        w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
            work_id: session.id.clone(),
            run_id: String::new(),
            kind: kind.into(),
            detail: detail.to_string(),
            ..Default::default()
        })),
    );
}

fn parent_key(world: &mut World, agent: Entity) -> Result<AgentKey> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    if key.is_work() {
        return Err(HostError::permission("not available to work agents"));
    }
    Ok(key)
}

#[allow(dead_code)]
fn _uses(_: Uuid, _: TurnState) {}

/// Marks a report as persisted only after the parent's accepted Aside is visible.
pub(crate) fn finalize_reports(world: &mut World) {
    if world
        .resource::<rig_ecs::session_log::SessionLog>()
        .failure()
        .is_some()
    {
        return;
    }
    let agents: Vec<AgentKey> = world
        .query::<(&AgentKey, &Conversation)>()
        .iter(world)
        .filter(|(key, _)| key.is_work())
        .map(|(key, _)| key.clone())
        .collect();
    for key in agents {
        let Some(work_id) = key.work_id() else {
            continue;
        };
        let Some(conversation) = world
            .query::<(&AgentKey, &Conversation)>()
            .iter(world)
            .find_map(|(candidate, conversation)| (candidate == &key).then_some(conversation))
        else {
            continue;
        };
        let Some((_, turn)) = conversation.turns().next_back() else {
            continue;
        };
        if turn.end.is_none() {
            continue;
        }
        let run = format!("{work_id}:{}", turn.start.get());
        let Some(parent) = chat::find_agent(world, &AgentKey::parent(key.companion())) else {
            continue;
        };
        if !chat::has_request(world, parent, &format!("report:{run}")) {
            continue;
        }
        let store = world.resource::<Store>().clone();
        let Ok(Some(mut session)) = store.work(work_id) else {
            continue;
        };
        if session.reported_run.as_deref() == Some(&run) {
            continue;
        }
        session.reported_run = Some(run);
        session.status = WorkStatus::Idle;
        session.updated_at = now();
        if let Err(error) = store.save_work(&session)
            && error.is_storage()
        {
            crate::host::fail(world, error.to_string());
        }
    }
}
