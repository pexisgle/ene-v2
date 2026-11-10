//! Conversation agent binding, input acceptance, idempotent delivery and history reading.
use super::{
    Host, Owner, client,
    db::{
        Receipt, ReceiptStatus, Result, digest, invalid, is_storage, not_found, now, unavailable,
        validate_id, validate_key,
    },
    mind, model, tick,
    work::WorkRecord,
};
use bevy_ecs::prelude::*;
use connectrpc::ConnectError;
use ene_rpc as w;
use rig_core::conversation::{Author, AuthorKind, MessageId, RequestId};
use rig_ecs::prelude::*;
use serde_json::json;
use uuid::Uuid;

pub(crate) const PARENT_PREFIX: &str = "parent-";
pub(crate) const MAX_INPUT: usize = 65_536;
const MAX_PENDING_INPUTS: usize = 32;

pub(crate) fn parent_id(companion_id: &str) -> String {
    format!("{PARENT_PREFIX}{companion_id}")
}

pub(crate) fn conversation_id(target: &w::Target) -> String {
    if target.work_id.is_empty() {
        parent_id(&target.companion_id)
    } else {
        target.work_id.clone()
    }
}

pub(crate) fn find_agent(world: &mut World, id: &str) -> Option<Entity> {
    world
        .query::<(Entity, &AgentId)>()
        .iter(world)
        .find_map(|(entity, agent)| (agent.0 == id).then_some(entity))
}

/// The agent of `target`, once the target itself is known to exist.
///
/// Reading and stopping must not create an agent: a read of an unknown
/// companion is `not_found`, and a stop of one is already done.
pub(crate) fn find_target(world: &mut World, target: &w::Target) -> Result<Option<Entity>> {
    validate_id(&target.companion_id)?;
    if target.work_id.is_empty() {
        mind::companion(&world.resource::<Host>().db, &target.companion_id)?;
    } else {
        validate_id(&target.work_id)?;
        let work = world
            .resource::<Host>()
            .db
            .get::<WorkRecord>(&format!("work:{}", target.work_id))?
            .ok_or_else(|| not_found("work not found"))?;
        if work.session.companion_id != target.companion_id {
            return Err(not_found("work not found"));
        }
    }
    let Some(entity) = find_agent(world, &conversation_id(target)) else {
        return Ok(None);
    };
    attach_agent(world, entity, target);
    Ok(Some(entity))
}

/// [`find_target`], spawning the agent when it does not exist yet.
pub(crate) fn ensure_target(world: &mut World, target: &w::Target) -> Result<Entity> {
    Ok(match find_target(world, target)? {
        Some(entity) => entity,
        None => spawn_agent(world, target),
    })
}

pub(crate) fn spawn_agent(world: &mut World, target: &w::Target) -> Entity {
    let entity = world
        .spawn((
            Agent,
            AgentId(conversation_id(target)),
            SystemPrompt(String::new()),
        ))
        .id();
    attach_agent(world, entity, target);
    entity
}

pub(crate) fn attach_agent(world: &mut World, entity: Entity, target: &w::Target) {
    if world.get::<Owner>(entity).is_none() {
        let owner = Owner(target.clone());
        let access = owner.tool_access();
        world.entity_mut(entity).insert((owner, access));
    }
    if !target.work_id.is_empty() {
        // A work agent's prompt is its brief, set when it starts.
        return;
    }
    let Ok(record) = mind::companion(&world.resource::<Host>().db, &target.companion_id) else {
        return;
    };
    // The author's mind is the agent's own prompt, not a per-call edit: it
    // is saved with the session and restored, so it is rebuilt from the DB
    // here every time an agent is attached.
    world
        .entity_mut(entity)
        .insert(SystemPrompt(record.companion.mind.clone()));
    if world.get::<AgentSections>(entity).is_none() {
        for (order, tag, text) in prompt_sections(&record) {
            world.spawn((PromptSection::new(order, tag, text), SectionOf(entity)));
        }
    }
}

/// The stable parts of a companion's system prompt, in the order they go.
fn prompt_sections(record: &mind::CompanionRecord) -> Vec<(i32, String, String)> {
    let mut sections = Vec::new();
    for (index, (tag, items)) in record.profile.sections().into_iter().enumerate() {
        if items.is_empty() {
            continue;
        }
        let text = items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push((PromptSection::ORDER_ROLE + index as i32, tag.into(), text));
    }
    sections.push((
        PromptSection::ORDER_PROJECT,
        "ene".into(),
        "作業報告には、ユーザーへ伝える必要がある分だけ答えてください。伝える必要がなければ空の返答にして構いません。"
            .into(),
    ));
    sections
}

/// Rewrites an agent's prompt after the author or its profile changed.
pub(crate) fn refresh_prompt(world: &mut World, companion_id: &str) {
    let Ok(record) = mind::companion(&world.resource::<Host>().db, companion_id) else {
        return;
    };
    let Some(agent) = find_agent(world, &parent_id(companion_id)) else {
        return;
    };
    world
        .entity_mut(agent)
        .insert(SystemPrompt(record.companion.mind.clone()));
    let sections = prompt_sections(&record);
    let existing: Vec<(Entity, String)> = world
        .query::<(Entity, &SectionOf, &PromptSection)>()
        .iter(world)
        .filter(|(_, of, _)| of.0 == agent)
        .map(|(entity, _, section)| (entity, section.tag.clone()))
        .collect();
    for (entity, tag) in existing {
        let Some((_, _, text)) = sections.iter().find(|(_, name, _)| *name == tag) else {
            continue;
        };
        if let Some(mut section) = world.get_mut::<PromptSection>(entity) {
            section.set_if_neq(PromptSection {
                text: text.clone(),
                ..section.clone()
            });
        }
    }
}

/// Binds `entity` to the model its companion assigns for `purpose`.
///
/// Called once per model call, not once per RPC: a turn can start with no
/// request behind it (a work report, a scheduled wake, a tool's delivery),
/// and none of those has an entry point to bind at. A binding that is still
/// the assigned one under the current settings is kept as it is.
pub(crate) fn ensure(
    world: &mut World,
    entity: Entity,
    companion_id: &str,
    purpose: model::Purpose,
) -> Result<()> {
    if let Some(binding) = world.get::<model::Binding>(entity)
        && model::unchanged(world.resource::<Host>(), companion_id, purpose, binding)?
    {
        return Ok(());
    }
    let host = world.resource::<Host>();
    let models = host.models.clone();
    let connected = models.connect(host, companion_id, purpose);
    // The old binding cannot stand in for the one the companion assigns now,
    // so it goes whether or not the new one connected.
    if let Some(old) = world.get::<model::Binding>(entity) {
        model::revoke(old);
    }
    world
        .entity_mut(entity)
        .remove::<(model::Binding, rig_ecs::model::Connection)>();
    let (connection, binding) = connected?;
    world.entity_mut(entity).insert((connection, binding));
    Ok(())
}

pub(crate) fn has_request(world: &mut World, entity: Entity, id: &str) -> bool {
    world.get::<Conversation>(entity).is_some_and(|c| {
        c.entries().any(|e| {
            e.author
                .and_then(|a| a.request.as_ref())
                .is_some_and(|r| r.0 == id)
        }) || c
            .asides()
            .any(|(_, a)| a.kind == "ene.accepted" && a.value.as_str() == Some(id))
    }) || world
        .query_filtered::<(&Incoming, &ForAgent), (With<Started>, Without<Outcome>)>()
        .iter(world)
        .any(|(input, agent)| {
            agent.0 == entity && input.author.request.as_ref().is_some_and(|r| r.0 == id)
        })
}

pub(crate) fn turn_outcome(end: Option<&TurnEnd>) -> (&'static str, Option<w::Failure>) {
    match end {
        None => ("running", None),
        Some(TurnEnd::Answered { .. }) => ("answered", None),
        Some(TurnEnd::Stopped { why, .. }) => (
            "stopped",
            why.as_deref().map(|why| w::Failure {
                code: "stopped".into(),
                message: crate::truncate_chars(why, 1024),
                ..Default::default()
            }),
        ),
        Some(TurnEnd::Failed { why }) => (
            "failed",
            Some(w::Failure {
                code: "failed".into(),
                message: crate::truncate_chars(why, 1024),
                ..Default::default()
            }),
        ),
    }
}

pub(crate) fn last_assistant(
    conversation: &Conversation,
    start: MessageId,
) -> Option<(u64, String)> {
    conversation
        .entries()
        .rev()
        .take_while(|entry| entry.id >= start)
        .find_map(|entry| match entry.message {
            message::Message::Assistant(assistant) => Some((entry.id.get(), assistant.text())),
            _ => None,
        })
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let agents: Vec<(Entity, String)> = world
        .query::<(Entity, &AgentId)>()
        .iter(world)
        .map(|(entity, id)| (entity, id.0.clone()))
        .collect();
    for (entity, id) in agents {
        let target = if let Some(companion) = id.strip_prefix(PARENT_PREFIX) {
            Some(w::Target {
                companion_id: companion.into(),
                ..Default::default()
            })
        } else {
            world
                .resource::<Host>()
                .db
                .get::<WorkRecord>(&format!("work:{id}"))?
                .map(|work| w::Target {
                    companion_id: work.session.companion_id,
                    work_id: id,
                    ..Default::default()
                })
        };
        if let Some(target) = target {
            attach_agent(world, entity, &target);
        } else {
            // Retiring records it in the session log, so a restart does not
            // restore an agent whose work is gone and retire it again.
            world.trigger(Retire { entity });
        }
    }

    for (key, mut receipt) in world.resource::<Host>().db.list::<Receipt>("receipt:")? {
        if receipt.status != ReceiptStatus::Pending {
            continue;
        }
        let target_id = key
            .strip_prefix("receipt:chat:")
            .or_else(|| key.strip_prefix("receipt:work:"))
            .and_then(|rest| rest.rsplit_once(':'))
            .map_or("", |(id, _)| id);
        let request_id = receipt.response["request_id"].as_str().unwrap_or_default();
        receipt.status = if !target_id.is_empty()
            && find_agent(world, target_id)
                .is_some_and(|entity| has_request(world, entity, request_id))
        {
            ReceiptStatus::Accepted
        } else {
            ReceiptStatus::NotDelivered
        };
        receipt.updated_at = now();
        world.resource::<Host>().db.put(&key, &receipt)?;
    }
    Ok(())
}

pub(crate) fn deliver_with_receipt(
    app: &mut App,
    entity: Entity,
    scope: &str,
    request_key: &str,
    hash: String,
    text: String,
) -> Result<String> {
    let pending = app
        .world_mut()
        .query_filtered::<&ForAgent, (With<Incoming>, Without<Settled>)>()
        .iter(app.world())
        .filter(|agent| agent.0 == entity)
        .count();
    if pending >= MAX_PENDING_INPUTS {
        return Err(ConnectError::resource_exhausted("inbox is full"));
    }
    let request_id = Uuid::new_v4().to_string();
    let key = format!("receipt:{scope}:{request_key}");
    let mut receipt = Receipt {
        hash,
        status: ReceiptStatus::Pending,
        response: json!({"request_id": request_id}),
        updated_at: now(),
    };
    app.world().resource::<Host>().db.put(&key, &receipt)?;
    let author = Author {
        request: Some(RequestId(request_id.clone())),
        ..Author::user()
    };
    app.world_mut()
        .trigger(Deliver::new(entity, text, DeliveryMode::Steer).with_author(author));
    tick(app);
    if app.world().resource::<Host>().failed.is_some()
        || !has_request(app.world_mut(), entity, &request_id)
    {
        receipt.status = ReceiptStatus::NotDelivered;
        app.world().resource::<Host>().db.put(&key, &receipt)?;
        return Err(unavailable("input was not saved"));
    }
    receipt.status = ReceiptStatus::Accepted;
    app.world().resource::<Host>().db.put(&key, &receipt)?;
    Ok(request_id)
}

pub(crate) fn accept(
    app: &mut App,
    request: w::SendMessageRequest,
) -> Result<w::MessageAcceptance> {
    let target = request
        .target
        .into_option()
        .ok_or_else(|| invalid("target is required"))?;
    validate_key(&request.request_key)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_INPUT {
        return Err(invalid("message must contain 1..65536 bytes"));
    }
    if !request.client_id.is_empty() {
        validate_id(&request.client_id)?;
    }
    let entity = ensure_target(app.world_mut(), &target)?;
    let scope = format!("chat:{}", conversation_id(&target));
    let hash = digest(format!("{}:{}", target.companion_id, request.text).as_bytes());
    if let Some(response) =
        app.world()
            .resource::<Host>()
            .db
            .check_receipt(&scope, &request.request_key, &hash)?
    {
        return Ok(w::MessageAcceptance {
            request_id: response["request_id"].as_str().unwrap_or_default().into(),
            ..Default::default()
        });
    }
    if target.work_id.is_empty() {
        if !request.client_id.is_empty() {
            client::activate_on_send(app.world_mut(), &target.companion_id, &request.client_id)?;
        }
        if let Err(error) =
            mind::inject_recall(app.world_mut(), entity, &target.companion_id, &request.text)
            && is_storage(&error)
        {
            return Err(error);
        }
    }
    let request_id = deliver_with_receipt(
        app,
        entity,
        &scope,
        &request.request_key,
        hash,
        request.text,
    )?;
    Ok(w::MessageAcceptance {
        request_id,
        ..Default::default()
    })
}

pub(crate) fn stop(app: &mut App, target: w::Target) -> Result<w::Empty> {
    // Stopping what does not exist, a companion that was deleted or a turn
    // that already ended, is already done: it is not an error.
    let entity = match find_target(app.world_mut(), &target) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Ok(w::Empty::default()),
        Err(error) if error.code == connectrpc::ErrorCode::NotFound => {
            return Ok(w::Empty::default());
        }
        Err(error) => return Err(error),
    };
    app.world_mut().trigger(StopTurn {
        entity,
        reason: Some("user stopped".into()),
    });
    tick(app);
    Ok(w::Empty::default())
}

pub(crate) fn history(world: &mut World, request: w::HistoryRequest) -> Result<w::History> {
    let target = request
        .target
        .into_option()
        .ok_or_else(|| invalid("target is required"))?;
    let Some(entity) = find_target(world, &target)? else {
        return Ok(w::History::default());
    };
    let cursor = request
        .cursor
        .as_deref()
        .and_then(|v| v.split(':').next())
        .and_then(|v| v.parse::<u64>().ok());
    let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
    let include_context = request.include_context.unwrap_or(false);
    let mut items = Vec::new();
    if let Some(conversation) = world.get::<Conversation>(entity) {
        for entry in conversation.entries() {
            if cursor.is_some_and(|c| entry.id.get() <= c) {
                continue;
            }
            if let Some(message) = to_message(&target, &entry, include_context) {
                items.push(message);
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    let next_cursor = items
        .last()
        .map(|m| format!("{}:{}", m.sequence, conversation_id(&target)));
    Ok(w::History {
        items,
        next_cursor,
        ..Default::default()
    })
}

pub(crate) fn message(world: &mut World, request: w::MessageRequest) -> Result<w::Message> {
    let target = request
        .target
        .into_option()
        .ok_or_else(|| invalid("target is required"))?;
    let id = request
        .message_id
        .parse::<u64>()
        .map_err(|_| invalid("invalid message id"))?;
    let entity = find_target(world, &target)?.ok_or_else(|| not_found("message not found"))?;
    world
        .get::<Conversation>(entity)
        .ok_or_else(|| not_found("message not found"))?
        .entries()
        .find(|entry| entry.id.get() == id)
        .and_then(|entry| to_message(&target, &entry, false))
        .ok_or_else(|| not_found("message not found"))
}

pub(crate) fn entry_text(message: &message::Message) -> Option<String> {
    match message {
        message::Message::System { .. } => None,
        message::Message::User { content } => Some(
            content
                .iter()
                .filter_map(|part| match part {
                    UserContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        message::Message::Assistant(assistant) => Some(assistant.text()),
    }
}

/// Who wrote an entry, as the API names it.
///
/// Host-injected context is not the user's speech, so it is `context`
/// rather than `user`; rig's own notes stay out of the history entirely.
pub(crate) fn role_of(entry: &rig_core::conversation::EntryRef<'_>) -> Option<&'static str> {
    match entry.message {
        message::Message::System { .. } => None,
        message::Message::Assistant(_) => Some("assistant"),
        message::Message::User { .. } => match entry.author.map(|author| &author.kind) {
            None | Some(AuthorKind::User) => Some("user"),
            Some(AuthorKind::Agent) => Some("agent"),
            Some(AuthorKind::Plugin(_)) => Some("context"),
            Some(AuthorKind::Rig) => None,
        },
    }
}

fn to_message(
    target: &w::Target,
    entry: &rig_core::conversation::EntryRef<'_>,
    include_context: bool,
) -> Option<w::Message> {
    let role = role_of(entry)?;
    if role == "context" && !include_context {
        return None;
    }
    let text = entry_text(entry.message)?;
    if text.is_empty() {
        return None;
    }
    Some(w::Message {
        id: entry.id.get().to_string(),
        companion_id: target.companion_id.clone(),
        work_id: target.work_id.clone(),
        role: role.into(),
        text,
        status: "completed".into(),
        created_at: entry.made.to_string(),
        sequence: entry.id.get(),
        request_id: entry
            .author
            .and_then(|a| a.request.as_ref())
            .map(|r| r.0.clone()),
        author: entry.author.map_or_else(String::new, |a| a.label()),
        ..Default::default()
    })
}
