//! Independent background work sessions, parent reporting and work tools.
use super::{
    Host, Owner, chat, client,
    db::{Result, aborted, digest, invalid, not_found, now, permission, unavailable, validate_key},
    kernel, tick,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const WORK_PROMPT: &str = "You are a work agent acting for a companion. Work from the supplied brief, report clearly, and do not expose the parent's private history.";
const MAX_INSTRUCTION: usize = 32 * 1024;
const INTERNAL_RIG: &str = "internal-rig";

/// How a work session runs. The host reads a session only through here, so
/// another backend (ACP, MCP, Wasm) is a new branch, not a rewrite.
#[derive(Clone, Copy)]
enum Backend {
    InternalRig,
}

impl Backend {
    fn parse(name: &str) -> Result<Self> {
        match name {
            INTERNAL_RIG => Ok(Self::InternalRig),
            _ => Err(unavailable("backend not supported")),
        }
    }
}

/// What a backend says about its session, in the host's own terms.
struct Views {
    runs: Vec<w::RunInfo>,
    last: Option<LastRun>,
}

struct LastRun {
    run_id: String,
    text: String,
    status: &'static str,
    failure: Option<w::Failure>,
}

impl Views {
    fn read(record: &WorkRecord, conversation: Option<&Conversation>) -> Result<Self> {
        match Backend::parse(&record.session.backend)? {
            Backend::InternalRig => {
                let mut runs = Vec::new();
                let mut last = None;
                if let Some(conversation) = conversation {
                    for (_, turn) in conversation.turns() {
                        let (state, failure) = chat::turn_outcome(turn.end.as_ref());
                        runs.push(w::RunInfo {
                            run_id: format!("{}:{}", record.session.id, turn.start.get()),
                            input_id: kernel::input_of(conversation, turn.start),
                            state: state.into(),
                            failure: failure.clone().into(),
                            ..Default::default()
                        });
                        if let Some(end) = &turn.end {
                            let (status, failure) = chat::turn_outcome(Some(end));
                            last = Some(LastRun {
                                run_id: format!("{}:{}", record.session.id, turn.start.get()),
                                text: chat::last_assistant(conversation, turn.start)
                                    .map(|(_, text)| text)
                                    .unwrap_or_default(),
                                status,
                                failure,
                            });
                        }
                    }
                }
                Ok(Self { runs, last })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkRecord {
    pub(crate) session: w::WorkSession,
    pub(crate) reported_run: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    prompt: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    work_id: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkArgs {
    work_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    text: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub(super) fn register_tools(app: &mut App) {
    app.add_open_tool::<StartArgs, _>(
        "start_work",
        "Start independent work and return its work id immediately.",
        ToolOptions::default(),
        kernel::run_tool(|world, agent, _, args| start_tool(world, agent, args)),
    )
    .add_open_tool::<SendArgs, _>(
        "send_work",
        "Send more instructions to an existing work session.",
        ToolOptions::default(),
        kernel::run_tool(|world, agent, _, args| send_tool(world, agent, args)),
    )
    .add_open_tool::<WorkArgs, _>(
        "stop_work",
        "Stop a work session. The parent conversation keeps running.",
        ToolOptions::default(),
        kernel::run_tool(|world, agent, _, args: WorkArgs| stop_tool(world, agent, &args.work_id)),
    )
    .add_open_tool::<Empty, _>(
        "list_work",
        "List this companion's work sessions.",
        ToolOptions {
            footprint: Some(Touches::Reads(Scope::Anything)),
        },
        kernel::run_tool(|world, agent, _, _: Empty| list_tool(world, agent)),
    )
    .add_open_tool::<ReportArgs, _>(
        "report",
        "Report progress or a result to the parent companion.",
        ToolOptions::default(),
        kernel::run_tool(|world, agent, _, args: ReportArgs| report_tool(world, agent, &args.text)),
    );
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    for (key, mut record) in world.resource::<Host>().db.list::<WorkRecord>("work:")? {
        if record.session.status == "running" {
            record.session.status = "interrupted".into();
            record.session.updated_at = now();
            world.resource::<Host>().db.put(&key, &record)?;
        }
        if let Some(child) = chat::find_agent(world, &record.session.id) {
            report_work(world, child)?;
        }
    }
    Ok(())
}

pub(crate) fn list(host: &Host, companion_id: &str) -> Result<Vec<w::WorkSession>> {
    Ok(host
        .db
        .list::<WorkRecord>("work:")?
        .into_iter()
        .filter(|(_, record)| record.session.companion_id == companion_id)
        .map(|(_, record)| record.session)
        .collect())
}

pub(crate) fn view(app: &mut App, id: &str) -> Result<w::WorkView> {
    let record = app
        .world()
        .resource::<Host>()
        .db
        .get::<WorkRecord>(&format!("work:{id}"))?
        .ok_or_else(|| not_found("work not found"))?;
    let child = chat::find_agent(app.world_mut(), id);
    let views = Views::read(
        &record,
        child.and_then(|child| app.world().get::<Conversation>(child)),
    )?;
    Ok(w::WorkView {
        session: record.session.into(),
        runs: views.runs,
        ..Default::default()
    })
}

pub(crate) fn send(app: &mut App, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    validate_key(&request.request_key)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_INSTRUCTION {
        return Err(invalid("invalid work instruction"));
    }
    let work = app
        .world()
        .resource::<Host>()
        .db
        .get::<WorkRecord>(&format!("work:{}", request.work_id))?
        .ok_or_else(|| not_found("work not found"))?;
    if work.session.closed {
        return Err(aborted("work is closed"));
    }
    Backend::parse(&work.session.backend)?;
    let scope = format!("work:{}", request.work_id);
    let hash = digest(format!("{}:{}", request.work_id, request.text).as_bytes());
    if let Some(response) =
        app.world()
            .resource::<Host>()
            .db
            .check_receipt(&scope, &request.request_key, &hash)?
    {
        return Ok(w::WorkAcceptance {
            request_id: response["request_id"].as_str().unwrap_or_default().into(),
            ..Default::default()
        });
    }
    if !request.client_id.is_empty() {
        client::activate_on_send(
            app.world_mut(),
            &work.session.companion_id,
            &request.client_id,
        )?;
    }
    let child = chat::find_agent(app.world_mut(), &request.work_id)
        .ok_or_else(|| not_found("work not found"))?;
    let request_id =
        chat::deliver_with_receipt(app, child, &scope, &request.request_key, hash, request.text)?;
    Ok(w::WorkAcceptance {
        request_id,
        ..Default::default()
    })
}

pub(crate) fn stop(app: &mut App, id: &str) -> Result<()> {
    let key = format!("work:{id}");
    let mut record = app
        .world()
        .resource::<Host>()
        .db
        .get::<WorkRecord>(&key)?
        .ok_or_else(|| not_found("work not found"))?;
    Backend::parse(&record.session.backend)?;
    if let Some(child) = chat::find_agent(app.world_mut(), id) {
        app.world_mut().trigger(StopTurn {
            entity: child,
            reason: Some("user stopped work".into()),
        });
    }
    record.session.status = "stopped".into();
    record.session.updated_at = now();
    app.world().resource::<Host>().db.put(&key, &record)?;
    notify(app.world_mut(), &record.session);
    tick(app);
    Ok(())
}

pub(crate) fn close(app: &mut App, id: &str) -> Result<()> {
    stop(app, id)?;
    let key = format!("work:{id}");
    let host = app.world().resource::<Host>();
    let mut record = host
        .db
        .get::<WorkRecord>(&key)?
        .ok_or_else(|| not_found("work not found"))?;
    record.session.closed = true;
    record.session.status = "closed".into();
    record.session.updated_at = now();
    host.db.put(&key, &record)?;
    notify(app.world_mut(), &record.session);
    Ok(())
}

pub(crate) fn report_work(world: &mut World, worker: Entity) -> Result<()> {
    let Some(owner) = world.get::<Owner>(worker).cloned() else {
        return Ok(());
    };
    let key = format!("work:{}", owner.work());
    let Some(mut record) = world.resource::<Host>().db.get::<WorkRecord>(&key)? else {
        return Ok(());
    };
    let views = Views::read(&record, world.get::<Conversation>(worker))?;
    let Some(last) = views.last else {
        return Ok(());
    };
    let run = last.run_id;
    if record.reported_run == run {
        return Ok(());
    }
    let (status, failure) = (last.status, last.failure);
    let text = last.text;
    let message = match failure {
        Some(failure) => format!(
            "作業 {} は {status} で終了しました（{}）。\n{text}",
            owner.work(),
            failure.message
        ),
        None => format!("作業 {} は {status} で終了しました。\n{text}", owner.work()),
    };
    let parent_id = chat::parent_id(owner.companion());
    let delivered = chat::find_agent(world, &parent_id)
        .is_some_and(|parent| chat::has_request(world, parent, &format!("report:{run}")));
    let changed = record.session.status != status
        || record.session.last_run_id.as_deref() != Some(&run)
        || record.session.last_report.as_deref() != Some(&message);
    if changed || delivered {
        record.session.status = status.into();
        record.session.last_run_id = Some(run.clone());
        record.session.last_report = Some(message.clone());
        if delivered {
            record.reported_run.clone_from(&run);
        }
        record.session.updated_at = now();
        world.resource::<Host>().db.put(&key, &record)?;
    }
    if !delivered {
        deliver_to_parent(world, owner.companion(), owner.work(), &message, Some(&run));
    }
    if changed {
        notify(world, &record.session);
    }
    Ok(())
}

fn start_tool(world: &mut World, agent: Entity, args: StartArgs) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    if args.prompt.trim().is_empty() || args.prompt.len() > MAX_INSTRUCTION {
        return Err(invalid("invalid work instructions"));
    }
    let companion_id = owner.companion().to_owned();
    let id = Uuid::new_v4().to_string();
    let target = w::Target {
        companion_id: companion_id.clone(),
        work_id: id.clone(),
        ..Default::default()
    };
    let record = WorkRecord {
        session: w::WorkSession {
            id: id.clone(),
            companion_id,
            backend: INTERNAL_RIG.into(),
            status: "running".into(),
            created_at: now(),
            updated_at: now(),
            ..Default::default()
        },
        reported_run: String::new(),
    };
    world
        .resource::<Host>()
        .db
        .put(&format!("work:{id}"), &record)?;
    let child = chat::spawn_agent(world, &target);
    world.entity_mut(child).insert(SystemPrompt(format!(
        "{WORK_PROMPT}\n\n作業: {}",
        args.prompt
    )));
    world.trigger(
        Deliver::new(child, args.prompt, DeliveryMode::Steer).with_author(Author::agent(
            owner.conversation_id(),
            Some(RequestId(id.clone())),
        )),
    );
    notify(world, &record.session);
    Ok(id)
}

fn send_tool(world: &mut World, agent: Entity, args: SendArgs) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    if args.text.trim().is_empty() || args.text.len() > MAX_INSTRUCTION {
        return Err(invalid("invalid work instruction"));
    }
    let key = format!("work:{}", args.work_id);
    let mut record = world
        .resource::<Host>()
        .db
        .get::<WorkRecord>(&key)?
        .ok_or_else(|| not_found("work not found"))?;
    if record.session.companion_id != owner.companion() || record.session.closed {
        return Err(not_found("work not found"));
    }
    let child =
        chat::find_agent(world, &args.work_id).ok_or_else(|| not_found("work not found"))?;
    world.trigger(
        Deliver::new(child, args.text, DeliveryMode::Steer).with_author(Author::agent(
            owner.conversation_id(),
            Some(RequestId(args.work_id.clone())),
        )),
    );
    record.session.status = "running".into();
    record.session.updated_at = now();
    world.resource::<Host>().db.put(&key, &record)?;
    notify(world, &record.session);
    Ok(args.work_id)
}

fn stop_tool(world: &mut World, agent: Entity, id: &str) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    let work = world
        .resource::<Host>()
        .db
        .get::<WorkRecord>(&format!("work:{id}"))?
        .ok_or_else(|| not_found("work not found"))?;
    if work.session.companion_id != owner.companion() {
        return Err(not_found("work not found"));
    }
    if let Some(child) = chat::find_agent(world, id) {
        world.trigger(StopTurn {
            entity: child,
            reason: Some("parent stopped work".into()),
        });
    }
    Ok(id.into())
}

fn list_tool(world: &mut World, agent: Entity) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    let items: Vec<Value> = list(world.resource::<Host>(), owner.companion())?
        .into_iter()
        .map(|session| serde_json::to_value(session).unwrap_or(Value::Null))
        .collect();
    Ok(Value::Array(items).to_string())
}

fn report_tool(world: &mut World, agent: Entity, text: &str) -> Result<String> {
    let owner = world
        .get::<Owner>(agent)
        .cloned()
        .ok_or_else(|| permission("unbound agent"))?;
    if !owner.is_work() {
        return Err(permission("only work agents report"));
    }
    if text.trim().is_empty() || text.len() > MAX_INSTRUCTION {
        return Err(invalid("invalid report"));
    }
    deliver_to_parent(world, owner.companion(), owner.work(), text, None);
    Ok("reported".into())
}

pub(crate) fn deliver_to_parent(
    world: &mut World,
    companion: &str,
    work: &str,
    text: &str,
    run: Option<&str>,
) {
    let target = w::Target {
        companion_id: companion.into(),
        ..Default::default()
    };
    let parent = chat::find_agent(world, &chat::parent_id(companion))
        .unwrap_or_else(|| chat::spawn_agent(world, &target));
    let request_id = run.map(|run| format!("report:{run}"));
    if request_id
        .as_deref()
        .is_some_and(|id| chat::has_request(world, parent, id))
    {
        return;
    }
    // `Queue` starts the parent's turn when it is idle: a finished work is
    // reported in the conversation, not only at the next user message.
    world.trigger(
        Deliver::new(parent, text, DeliveryMode::Queue)
            .with_author(Author::agent(work, request_id.map(RequestId))),
    );
}

pub(crate) fn notify(world: &mut World, session: &w::WorkSession) {
    world.resource_mut::<Host>().notify(
        Some(session.companion_id.clone()),
        None,
        w::notification::Event::WorkUpdated(Box::new(w::WorkUpdated {
            session: session.clone().into(),
            ..Default::default()
        })),
    );
}
