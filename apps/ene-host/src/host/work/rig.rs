//! The built-in Rig backend: starting a Run and delivering reports.
use super::{MAX_INSTRUCTION, WORK_PROMPT, WorkSession, WorkStatus};
use crate::host::{
    agent::AgentKey,
    chat,
    db::{Result, Store, new_id, now},
    error::HostError,
    notify::Notifier,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use serde_json::{Value, json};

pub(super) fn start_in_world(
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
        Deliver::new(child, instruction, DeliveryMode::Queue).with_author(Author::agent(
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

pub(super) fn deliver_to_parent(
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

pub(super) fn notify(world: &mut World, session: &WorkSession, kind: &str, detail: Value) {
    notify_run(world, &session.companion_id, &session.id, "", kind, detail);
}

pub(super) fn notify_run(
    world: &mut World,
    companion: &str,
    work: &str,
    run: &str,
    kind: &str,
    detail: Value,
) {
    world.resource_mut::<Notifier>().push(
        Some(companion.into()),
        None,
        w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
            work_id: work.into(),
            run_id: run.into(),
            kind: kind.into(),
            detail: detail.to_string(),
            ..Default::default()
        })),
    );
}

pub(super) fn current_run(world: &World, agent: Entity, work: &str) -> String {
    world
        .get::<Conversation>(agent)
        .and_then(|conversation| conversation.turns().next_back())
        .map_or_else(String::new, |(_, turn)| {
            format!("{work}:{}", turn.start.get())
        })
}

pub(super) fn parent_key(world: &mut World, agent: Entity) -> Result<AgentKey> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    if key.is_work() {
        return Err(HostError::permission("not available to work agents"));
    }
    Ok(key)
}
