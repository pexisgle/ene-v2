//! Work sessions, their runs and the parent report.
mod rig;
mod tools;

pub(crate) use tools::register_tools;
#[cfg(test)]
pub(crate) use tools::stop_tool;

use super::{
    agent::AgentKey,
    chat, client,
    db::{Result, Store, digest, now, validate_key},
    error::HostError,
    tick, wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig::{deliver_to_parent, notify, start_in_world};
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use serde_json::json;

pub(super) const WORK_PROMPT: &str = "You are a work agent acting for a companion. Work from the supplied brief, report clearly, and do not expose the parent's private history.";
pub(super) const MAX_INSTRUCTION: usize = 32 * 1024;

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

pub(crate) fn finalize_reports(world: &mut World, companion: &str) -> Result<()> {
    if world
        .resource::<rig_ecs::session_log::SessionLog>()
        .failure()
        .is_some()
    {
        return Ok(());
    }
    let Some(parent) = chat::find_agent(world, &AgentKey::parent(companion)) else {
        return Ok(());
    };
    let store = world.resource::<Store>().clone();
    for mut session in store.works_for(companion)? {
        if session.closed {
            continue;
        }
        let Some(child) = chat::find_agent(world, &AgentKey::work(companion, &session.id)) else {
            continue;
        };
        let Some(run) = world.get::<Conversation>(child).and_then(|conversation| {
            let (_, turn) = conversation.turns().next_back()?;
            turn.end.as_ref()?;
            Some(format!("{}:{}", session.id, turn.start.get()))
        }) else {
            continue;
        };
        if session.reported_run.as_deref() == Some(&run) {
            continue;
        }
        if !chat::has_request(world, parent, &format!("report:{run}")) {
            continue;
        }
        session.reported_run = Some(run);
        session.status = WorkStatus::Idle;
        session.updated_at = now();
        store.save_work(&session)?;
    }
    Ok(())
}
