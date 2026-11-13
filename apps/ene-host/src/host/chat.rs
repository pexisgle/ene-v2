//! Chat: idempotent acceptance, stop and the history projection.
use super::{
    agent,
    db::{Result, Store, digest, now},
    error::HostError,
};
use ene_rpc as w;
use rig_core::conversation::{Aside, Author, RequestId};
use rig_ecs::prelude::*;
use serde_json::json;

/// The ingress limits from docs/design/compaction.md §5.1.
pub(crate) const MAX_INPUT_TOKENS: usize = 8_000;
pub(crate) const MAX_INPUT_CHARS: usize = 32_000;

/// Accepts a user message. The returned id is only produced after the message
/// is in the session log; the same request key returns the same id.
pub(crate) fn accept(app: &mut App, request: w::SendMessageRequest) -> Result<w::Acceptance> {
    if request.text.trim().is_empty() {
        return Err(HostError::invalid("a message is required"));
    }
    if request.text.chars().count() > MAX_INPUT_CHARS {
        return Err(HostError::invalid("the message is too large"));
    }
    if estimate_tokens(&request.text) > MAX_INPUT_TOKENS {
        return Err(HostError::invalid("the message is too large"));
    }
    super::db::validate_id(&request.companion_id)?;
    if !request.request_key.is_empty() {
        super::db::validate_id(&request.request_key)?;
    }
    let text = request.text.clone();
    let hash = digest(format!("{}:{}", request.companion_id, text).as_bytes());
    let world = app.world_mut();
    // Only an explicit user action resets the compaction idle clock.
    super::compaction::note_user_activity(world);
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let agent = agent::agent_of(world, &request.companion_id)
        .ok_or_else(|| HostError::not_found("companion agent not found"))?;
    agent::ensure_model(world, agent)?;

    // A receipt is only trusted when the input it names is actually in the
    // conversation; an orphan receipt is ignored and a new accept proceeds.
    if !request.request_key.is_empty() {
        for (key, request_id, recorded) in receipts(world, agent) {
            if key != request.request_key {
                continue;
            }
            if recorded != hash {
                return Err(HostError::already_exists(
                    "this request key already accepted different content",
                ));
            }
            if has_input(world, agent, &request_id) {
                return Ok(w::Acceptance {
                    request_id,
                    ..Default::default()
                });
            }
        }
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let author = Author {
        request: Some(RequestId(request_id.clone())),
        ..Author::user()
    };
    world.trigger(Deliver::new(agent, text, DeliveryMode::Steer).with_author(author));
    super::tick(app);
    if let Some(reason) = app
        .world()
        .resource::<super::resources::HostState>()
        .failed
        .clone()
    {
        return Err(HostError::unavailable(reason));
    }
    if !has_input(app.world_mut(), agent, &request_id) {
        return Err(HostError::unavailable("the input was not saved"));
    }
    let world = app.world_mut();
    if let Some(mut conversation) = world.get_mut::<Conversation>(agent) {
        conversation.add_aside(Aside::new(
            "ene.accepted",
            Author::plugin("ene-host"),
            json!({
                "request_key": request.request_key,
                "request_id": request_id,
                "hash": hash,
                "accepted_at": now(),
            }),
        ));
    }
    super::tick(app);
    Ok(w::Acceptance {
        request_id,
        ..Default::default()
    })
}

fn receipts(world: &World, agent: Entity) -> Vec<(String, String, Vec<u8>)> {
    let Some(conversation) = world.get::<Conversation>(agent) else {
        return Vec::new();
    };
    conversation
        .asides()
        .filter(|(_, aside)| aside.kind == "ene.accepted")
        .filter_map(|(_, aside)| {
            Some((
                aside.value.get("request_key")?.as_str()?.to_owned(),
                aside.value.get("request_id")?.as_str()?.to_owned(),
                aside
                    .value
                    .get("hash")?
                    .as_str()
                    .map(|hash| hash.as_bytes().to_vec())
                    .or_else(|| {
                        aside
                            .value
                            .get("hash")
                            .and_then(|hash| hash.as_array())
                            .map(|bytes| {
                                bytes
                                    .iter()
                                    .filter_map(serde_json::Value::as_u64)
                                    .map(|byte| byte as u8)
                                    .collect()
                            })
                    })?,
            ))
        })
        .collect()
}

/// A conservative character-based token estimate; the provider counts tokens
/// itself and the host only needs a safe margin.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

fn has_input(world: &World, agent: Entity, request_id: &str) -> bool {
    let Some(conversation) = world.get::<Conversation>(agent) else {
        return false;
    };
    conversation.entries().any(|entry| {
        entry
            .author
            .and_then(|author| author.request.as_ref())
            .is_some_and(|request| request.0 == request_id && entry.is_request())
    })
}

/// Stops the current turn.
pub(crate) fn stop(world: &mut World, request: w::StopRequest) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    if let Some(agent) = agent::agent_of(world, &request.companion_id) {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some("the user stopped the turn".into()),
        });
    }
    // Stop means stop speaking too; stale playback reports are discarded.
    let _ = super::voice::flush(world, &request.companion_id, w::FlushReason::Cancelled);
    Ok(())
}

/// The conversation transcript, oldest first, paged by message id.
pub(crate) fn history(world: &mut World, request: w::HistoryRequest) -> Result<w::History> {
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let agent = agent::agent_of(world, &request.companion_id)
        .ok_or_else(|| HostError::not_found("companion agent not found"))?;
    let after = request
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<u64>().ok())
        .map(|after| after + 1);
    let limit = request.limit.unwrap_or(100).clamp(1, 200) as usize;
    let mut items = Vec::new();
    if let Some(conversation) = world.get::<Conversation>(agent) {
        for entry in conversation.entries() {
            if after.is_some_and(|after| entry.id.get() < after) {
                continue;
            }
            let Some(text) = agent::entry_text(&entry) else {
                continue;
            };
            items.push(message(&request.companion_id, "", &entry, &text));
        }
    }
    if items.len() > limit {
        items = items.split_off(items.len() - limit);
    }
    Ok(w::History {
        items,
        ..Default::default()
    })
}

/// One message by id.
pub(crate) fn message_by_id(world: &mut World, request: w::MessageRequest) -> Result<w::Message> {
    let agent = agent::agent_of(world, &request.companion_id)
        .ok_or_else(|| HostError::not_found("companion agent not found"))?;
    let wanted = request
        .message_id
        .parse::<u64>()
        .map_err(|_| HostError::invalid("message id is not a number"))?;
    let conversation = world
        .get::<Conversation>(agent)
        .ok_or_else(|| HostError::not_found("message not found"))?;
    for entry in conversation.entries() {
        if entry.id.get() != wanted {
            continue;
        }
        let Some(text) = agent::entry_text(&entry) else {
            continue;
        };
        return Ok(message(&request.companion_id, "", &entry, &text));
    }
    Err(HostError::not_found("message not found"))
}

fn message(
    companion: &str,
    work: &str,
    entry: &rig_core::conversation::EntryRef<'_>,
    text: &str,
) -> w::Message {
    w::Message {
        id: entry.id.get().to_string(),
        companion_id: companion.to_owned(),
        work_id: work.to_owned(),
        role: role(entry).into(),
        text: text.to_owned(),
        created_at: entry.made.to_string(),
        sequence: entry.id.get(),
        request_id: entry
            .author
            .and_then(|author| author.request.as_ref())
            .map(|request| request.0.clone()),
        author: entry
            .author
            .map(|author| author.label())
            .unwrap_or_default(),
        ..Default::default()
    }
}

fn role(entry: &rig_core::conversation::EntryRef<'_>) -> w::MessageRole {
    use rig_core::conversation::AuthorKind;
    match (entry.author.map(|author| &author.kind), &entry.message) {
        (None, rig_core::completion::Message::User { .. }) => w::MessageRole::User,
        (None, _) => w::MessageRole::Assistant,
        (Some(AuthorKind::User), _) => w::MessageRole::User,
        (Some(AuthorKind::Agent), _) => w::MessageRole::Agent,
        _ => w::MessageRole::Context,
    }
}
