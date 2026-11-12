//! Conversation binding, input acceptance, idempotent delivery and history.
use super::{
    agent::AgentKey,
    client,
    db::{Result, Store, digest, validate_id, validate_key},
    error::HostError,
    failure::Failure,
    model::{self, Binding},
    resources::{HostState, Inactivity, Models},
    tick,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Aside, Author, AuthorKind, MessageId, RequestId, TurnEnd};
use rig_ecs::prelude::*;
use serde_json::json;
use uuid::Uuid;

pub(crate) const MAX_INPUT: usize = 65_536;
const MAX_PENDING_INPUTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnState {
    Running,
    Answered,
    Stopped,
    Failed,
}

impl TurnState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "streaming",
            Self::Answered => "answered",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

pub(crate) const PARENT_TOOLS: &[&str] = &[
    "current_time",
    "mind_recall",
    "mind_read",
    "mind_update",
    "start_work",
    "send_work",
    "stop_work",
    "list_works",
    "set_eye_watch",
    "body_motion",
    "noop",
    "device_request",
];
pub(crate) const WORK_TOOLS: &[&str] = &["current_time", "report", "device_request"];

pub(crate) fn tool_access(key: &AgentKey) -> ToolAccess {
    let names = if key.is_work() {
        WORK_TOOLS
    } else {
        PARENT_TOOLS
    };
    ToolAccess::Only(names.iter().map(|name| (*name).to_owned()).collect())
}

pub(crate) fn find_agent(world: &mut World, key: &AgentKey) -> Option<Entity> {
    world
        .query::<(Entity, &AgentKey)>()
        .iter(world)
        .find_map(|(entity, found)| (found == key).then_some(entity))
}

pub(crate) fn find_target(world: &mut World, key: &AgentKey) -> Result<Option<Entity>> {
    let store = world.resource::<Store>();
    match key {
        AgentKey::Parent { companion } => {
            store
                .companion(companion)?
                .ok_or_else(|| HostError::not_found("companion not found"))?;
        }
        AgentKey::Work { companion, work } => {
            let session = store
                .work(work)?
                .ok_or_else(|| HostError::not_found("work not found"))?;
            if session.companion_id != *companion {
                return Err(HostError::not_found("work not found"));
            }
        }
    }
    let Some(entity) = find_agent(world, key) else {
        return Ok(None);
    };
    attach_agent(world, entity, key);
    Ok(Some(entity))
}

pub(crate) fn ensure_target(world: &mut World, key: &AgentKey) -> Result<Entity> {
    Ok(match find_target(world, key)? {
        Some(entity) => entity,
        None => spawn_agent(world, key),
    })
}

pub(crate) fn spawn_agent(world: &mut World, key: &AgentKey) -> Entity {
    let entity = world
        .spawn((
            Agent,
            AgentId(key.agent_id()),
            key.clone(),
            SystemPrompt(String::new()),
        ))
        .id();
    attach_agent(world, entity, key);
    entity
}

pub(crate) fn attach_agent(world: &mut World, entity: Entity, key: &AgentKey) {
    world
        .entity_mut(entity)
        .insert((key.clone(), tool_access(key)));
    if key.is_work() {
        return;
    }
    let Ok(Some(companion)) = world.resource::<Store>().companion(key.companion()) else {
        return;
    };
    world
        .entity_mut(entity)
        .insert(SystemPrompt(companion.mind.clone()));
    set_sections(world, entity, &companion);
}

fn set_sections(world: &mut World, agent: Entity, companion: &super::companion::Companion) {
    let existing: Vec<Entity> = world
        .query::<(Entity, &SectionOf)>()
        .iter(world)
        .filter(|(_, section)| section.0 == agent)
        .map(|(entity, _)| entity)
        .collect();
    for entity in existing {
        let _ = world.despawn(entity);
    }
    for (order, tag, text) in prompt_sections(companion) {
        world.spawn((PromptSection::new(order, tag, text), SectionOf(agent)));
    }
}

fn prompt_sections(companion: &super::companion::Companion) -> Vec<(i32, String, String)> {
    let mut sections = Vec::new();
    for (index, (tag, items)) in companion.profile.sections().into_iter().enumerate() {
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

pub(crate) fn refresh_prompt(world: &mut World, companion_id: &str) {
    let Ok(Some(companion)) = world.resource::<Store>().companion(companion_id) else {
        return;
    };
    let Some(agent) = find_agent(world, &AgentKey::parent(companion_id)) else {
        return;
    };
    world
        .entity_mut(agent)
        .insert(SystemPrompt(companion.mind.clone()));
    set_sections(world, agent, &companion);
}

pub(crate) fn ensure_model(
    world: &mut World,
    entity: Entity,
    companion_id: &str,
    purpose: model::Purpose,
) -> Result<()> {
    if let Some(binding) = world.get::<Binding>(entity)
        && model::unchanged(world.resource::<Store>(), companion_id, purpose, binding)?
    {
        return Ok(());
    }
    let models = world.resource::<Models>().0.clone();
    let store = world.resource::<Store>().clone();
    if let Some(old) = world.get::<Binding>(entity) {
        model::revoke(old);
    }
    world
        .entity_mut(entity)
        .remove::<(Binding, rig_ecs::model::Connection)>();
    let (connection, binding) = models.connect(&store, companion_id, purpose)?;
    world.entity_mut(entity).insert((connection, binding));
    Ok(())
}

pub(crate) fn has_input(world: &mut World, entity: Entity, id: &str) -> bool {
    world
        .get::<Conversation>(entity)
        .is_some_and(|conversation| {
            conversation.entries().any(|entry| {
                entry
                    .author
                    .and_then(|author| author.request.as_ref())
                    .is_some_and(|request| request.0 == id)
            })
        })
        || world
            .get::<Conversation>(entity)
            .is_some_and(|conversation| {
                conversation.asides().any(|(_, aside)| {
                    aside.kind == "ene.turn_input"
                        && aside.value.get("input_id").and_then(|value| value.as_str()) == Some(id)
                })
            })
        || world
            .query_filtered::<(&Incoming, &ForAgent), (With<Started>, Without<Outcome>)>()
            .iter(world)
            .any(|(input, agent)| {
                agent.0 == entity
                    && input
                        .author
                        .request
                        .as_ref()
                        .is_some_and(|request| request.0 == id)
            })
}

pub(crate) fn has_request(world: &mut World, entity: Entity, id: &str) -> bool {
    has_input(world, entity, id)
        || world
            .get::<Conversation>(entity)
            .is_some_and(|conversation| {
                conversation.asides().any(|(_, aside)| {
                    aside.kind == "ene.accepted"
                        && (aside.value.as_str() == Some(id)
                            || aside
                                .value
                                .get("request_id")
                                .and_then(|value| value.as_str())
                                == Some(id))
                })
            })
}

pub(crate) fn turn_state(end: Option<&TurnEnd>) -> (TurnState, Option<Failure>) {
    match end {
        None => (TurnState::Running, None),
        Some(TurnEnd::Answered { .. }) => (TurnState::Answered, None),
        Some(TurnEnd::Stopped { why, .. }) => (
            TurnState::Stopped,
            why.as_deref()
                .map(|why| Failure::new("stopped", crate::truncate_chars(why, 1024))),
        ),
        Some(TurnEnd::Failed { why }) => (
            TurnState::Failed,
            Some(Failure::new("failed", crate::truncate_chars(why, 1024))),
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

pub(crate) fn input_of(conversation: &Conversation, start: MessageId) -> Option<String> {
    conversation
        .request(start)
        .and_then(|entry| entry.author)
        .and_then(|author| author.request.as_ref())
        .map(|request| request.0.clone())
        .or_else(|| {
            conversation.asides().find_map(|(_, aside)| {
                (aside.kind == "ene.turn_input"
                    && aside.value["start"].as_u64() == Some(start.get()))
                .then(|| aside.value["input_id"].as_str().map(str::to_owned))
                .flatten()
            })
        })
}

pub(crate) fn accept(app: &mut App, request: w::SendMessageRequest) -> Result<w::Acceptance> {
    validate_id(&request.companion_id)?;
    validate_key(&request.request_key)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_INPUT {
        return Err(HostError::invalid("message must contain 1..65536 bytes"));
    }
    if !request.client_id.is_empty() {
        validate_id(&request.client_id)?;
    }
    let key = AgentKey::parent(request.companion_id);
    let entity = ensure_target(app.world_mut(), &key)?;
    let hash = digest(format!("{}:{}", key.companion(), request.text).as_bytes());
    if let Some(accepted) = accepted(app.world_mut(), entity, &request.request_key) {
        if accepted.hash != hash {
            return Err(HostError::already_exists(
                "request key was used with different content",
            ));
        }
        return Ok(w::Acceptance {
            request_id: accepted.request_id,
            ..Default::default()
        });
    }
    if !request.client_id.is_empty() {
        client::activate_on_send(app.world_mut(), key.companion(), &request.client_id)?;
    }
    let now = app.world().resource::<Time<Virtual>>().elapsed();
    app.world_mut()
        .resource_mut::<Inactivity>()
        .0
        .insert(key.companion().into(), now);
    let request_id = deliver_accepted(app, entity, &request.request_key, hash, request.text)?;
    Ok(w::Acceptance {
        request_id,
        ..Default::default()
    })
}

pub(crate) fn deliver_accepted(
    app: &mut App,
    entity: Entity,
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
        return Err(HostError::exhausted("inbox is full"));
    }
    let request_id = Uuid::new_v4().to_string();
    let author = Author {
        request: Some(RequestId(request_id.clone())),
        ..Author::user()
    };
    app.world_mut()
        .trigger(Deliver::new(entity, text, DeliveryMode::Steer).with_author(author));
    let aside = record_accepted(app.world_mut(), entity, request_key, &hash, &request_id);
    tick(app);
    let failed = app.world().resource::<HostState>().failed.is_some();
    if failed || !has_input(app.world_mut(), entity, &request_id) {
        if let (Some(aside), Some(mut conversation)) =
            (aside, app.world_mut().get_mut::<Conversation>(entity))
        {
            conversation.remove_aside(aside);
        }
        return Err(HostError::unavailable("input was not saved"));
    }
    Ok(request_id)
}

pub(crate) fn accepted(world: &mut World, entity: Entity, request_key: &str) -> Option<Accepted> {
    let candidates: Vec<(String, String)> = world
        .get::<Conversation>(entity)
        .map(|conversation| {
            conversation
                .asides()
                .filter_map(|(_, aside)| {
                    (aside.kind == "ene.accepted"
                        && aside
                            .value
                            .get("request_key")
                            .and_then(|value| value.as_str())
                            == Some(request_key))
                    .then(|| {
                        Some((
                            aside.value.get("hash")?.as_str()?.to_owned(),
                            aside.value.get("request_id")?.as_str()?.to_owned(),
                        ))
                    })
                    .flatten()
                })
                .collect()
        })
        .unwrap_or_default();
    candidates
        .into_iter()
        .find(|(_, request_id)| has_input(world, entity, request_id))
        .map(|(hash, request_id)| Accepted { hash, request_id })
}

pub(crate) struct Accepted {
    pub(crate) hash: String,
    pub(crate) request_id: String,
}

pub(crate) fn record_accepted(
    world: &mut World,
    entity: Entity,
    request_key: &str,
    hash: &str,
    request_id: &str,
) -> Option<rig_core::conversation::AsideId> {
    let mut conversation = world.get_mut::<Conversation>(entity)?;
    Some(conversation.add_aside(Aside::new(
        "ene.accepted",
        Author::rig(),
        json!({
            "request_key": request_key,
            "hash": hash,
            "request_id": request_id,
        }),
    )))
}

pub(crate) fn stop(app: &mut App, companion_id: &str) -> Result<()> {
    let key = AgentKey::parent(companion_id);
    let entity = match find_target(app.world_mut(), &key) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Ok(()),
        Err(HostError::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    app.world_mut().trigger(StopTurn {
        entity,
        reason: Some("user stopped".into()),
    });
    tick(app);
    Ok(())
}

pub(crate) fn history(world: &mut World, request: w::HistoryRequest) -> Result<w::History> {
    validate_id(&request.companion_id)?;
    let key = AgentKey::parent(request.companion_id.clone());
    world
        .resource::<Store>()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let Some(entity) = find_agent(world, &key) else {
        return Ok(w::History::default());
    };
    let cursor = request
        .cursor
        .as_deref()
        .and_then(|value| value.split(':').next())
        .and_then(|value| value.parse::<u64>().ok());
    let limit = request.limit.unwrap_or(50).clamp(1, 200) as usize;
    let include_context = request.include_context.unwrap_or(false);
    let mut items = Vec::new();
    if let Some(conversation) = world.get::<Conversation>(entity) {
        for entry in conversation.entries() {
            if cursor.is_some_and(|cursor| entry.id.get() <= cursor) {
                continue;
            }
            if let Some(message) = to_message(&key, &entry, include_context) {
                items.push(message);
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(w::History {
        items,
        ..Default::default()
    })
}

pub(crate) fn message(world: &mut World, request: w::MessageRequest) -> Result<w::Message> {
    validate_id(&request.companion_id)?;
    let key = AgentKey::parent(request.companion_id.clone());
    let id = request
        .message_id
        .parse::<u64>()
        .map_err(|_| HostError::invalid("invalid message id"))?;
    world
        .resource::<Store>()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let entity =
        find_agent(world, &key).ok_or_else(|| HostError::not_found("message not found"))?;
    world
        .get::<Conversation>(entity)
        .ok_or_else(|| HostError::not_found("message not found"))?
        .entries()
        .find(|entry| entry.id.get() == id)
        .and_then(|entry| to_message(&key, &entry, true))
        .ok_or_else(|| HostError::not_found("message not found"))
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
    target: &AgentKey,
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
        companion_id: target.companion().into(),
        work_id: target.work_id().unwrap_or_default().into(),
        role: role.into(),
        text,
        created_at: entry.made.to_string(),
        sequence: entry.id.get(),
        request_id: entry
            .author
            .and_then(|author| author.request.as_ref())
            .map(|request| request.0.clone()),
        author: entry
            .author
            .map_or_else(String::new, |author| author.label()),
        ..Default::default()
    })
}

pub(crate) fn ensure_parent(world: &mut World, companion: &str) -> Entity {
    let key = AgentKey::parent(companion);
    find_agent(world, &key).unwrap_or_else(|| spawn_agent(world, &key))
}
