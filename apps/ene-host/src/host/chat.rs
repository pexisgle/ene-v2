//! Chat: idempotent acceptance, stop and the history projection.
use super::{
    agent,
    db::{Result, Store},
    error::HostError,
};
use ene_rpc as w;
use ene_store::util::{digest, now};
use rig_core::conversation::{Aside, Author, AuthorKind, EntryRef, RequestId};
use rig_ecs::prelude::*;
use serde::Deserialize;
use serde_json::json;

/// The ingress limits from docs/design/mind.md.
pub(crate) const MAX_INPUT_TOKENS: usize = 8_000;
pub(crate) const MAX_INPUT_CHARS: usize = 32_000;

/// Accepts a user message. The returned id is only produced after the message
/// is in the session log; the same request key returns the same id.
#[tracing::instrument(
    name = "chat.accept",
    skip_all,
    fields(companion_id = %request.companion_id, request_id = tracing::field::Empty)
)]
pub(crate) fn accept(app: &mut App, request: &w::SendMessageRequest) -> Result<w::Acceptance> {
    if request.text.trim().is_empty() {
        return Err(HostError::Invalid(("a message is required").into()));
    }
    if request.text.chars().count() > MAX_INPUT_CHARS {
        return Err(HostError::Invalid(("the message is too large").into()));
    }
    if rig_memory::estimate_tokens(&[rig_core::completion::Message::user(&request.text)])
        > MAX_INPUT_TOKENS as u64
    {
        return Err(HostError::Invalid(("the message is too large").into()));
    }
    ene_domain::validate_id(&request.companion_id)?;
    if !request.request_key.is_empty() {
        ene_domain::validate_id(&request.request_key)?;
    }
    let hash = digest(format!("{}:{}", request.companion_id, request.text).as_bytes());
    let world = app.world_mut();
    let store = world.resource::<Store>().clone();
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let agent = agent::agent_of(world, &request.companion_id)
        .ok_or_else(|| HostError::NotFound(("companion agent not found").into()))?;

    // A receipt is only trusted when the input it names is actually in the
    // conversation; an orphan receipt is ignored and a new accept proceeds.
    if !request.request_key.is_empty()
        && let Some(request_id) = accepted(world, agent, &request.request_key, &hash)?
    {
        tracing::Span::current().record("request_id", request_id.as_str());
        return Ok(w::Acceptance {
            request_id,
            ..Default::default()
        });
    }
    agent::ensure_model(world, agent)?;

    let request_id = uuid::Uuid::new_v4().to_string();
    tracing::Span::current().record("request_id", request_id.as_str());
    // One frame carries the message: the receipt is in the conversation before
    // the frame saves it, and the input reaches the same conversation in the
    // frame. A crash between the two saves leaves an orphan receipt, which the
    // check above ignores.
    {
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
        let author = Author {
            request: Some(RequestId(request_id.clone())),
            ..Author::user()
        };
        let mode = super::permission::user_reply_delivery(world.get_resource(), agent);
        world.trigger(Deliver::new(agent, request.text.clone(), mode).with_author(author));
    }
    super::tick(app);
    if let Some(reason) = app
        .world()
        .resource::<super::resources::HostState>()
        .failure()
    {
        return Err(HostError::Unavailable(reason));
    }
    if !has_input(app.world(), agent, &request_id) {
        return Err(HostError::Unavailable(("the input was not saved").into()));
    }
    super::permission::consider_user_reply(app.world_mut(), agent, &request_id, &request.text);
    Ok(w::Acceptance {
        request_id,
        ..Default::default()
    })
}

/// The request id a retried `key` names, when a confirmed receipt binds it.
///
/// A receipt whose input is not in the conversation is an orphan: it was
/// written by an accept that did not save its message, so it binds no content
/// and is skipped, even when the retry carries a different message. `Err` means
/// a confirmed receipt accepted different content.
fn accepted(world: &World, agent: Entity, key: &str, hash: &[u8]) -> Result<Option<String>> {
    let Some(conversation) = world.get::<Conversation>(agent) else {
        return Ok(None);
    };
    for (_, aside) in conversation.asides() {
        let value = &aside.value;
        if aside.kind != "ene.accepted"
            || !matches!(&aside.author.kind, AuthorKind::Plugin(id) if id == "ene-host")
            || value.get("request_key").and_then(serde_json::Value::as_str) != Some(key)
        {
            continue;
        }
        let Some(request_id) = value.get("request_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(recorded) = value
            .get("hash")
            .and_then(|value| Vec::<u8>::deserialize(value).ok())
        else {
            continue;
        };
        if !has_input(world, agent, request_id) {
            continue;
        }
        if recorded != hash {
            return Err(HostError::AlreadyExists(
                ("this request key already accepted different content").into(),
            ));
        }
        return Ok(Some(request_id.to_owned()));
    }
    Ok(None)
}

fn has_input(world: &World, agent: Entity, request_id: &str) -> bool {
    let in_conversation = world
        .get::<Conversation>(agent)
        .is_some_and(|conversation| {
            conversation.entries().any(|entry| {
                entry
                    .author
                    .and_then(|author| author.request.as_ref())
                    .is_some_and(|request| request.0 == request_id && entry.is_request())
            })
        });
    if in_conversation {
        return true;
    }
    world.iter_entities().any(|entity| {
        let Some(target) = entity.get::<ForAgent>() else {
            return false;
        };
        let Some(message) = entity.get::<Incoming>() else {
            return false;
        };
        target.0 == agent
            && message
                .author
                .request
                .as_ref()
                .is_some_and(|request| request.0 == request_id)
    })
}

/// Stops the current turn.
pub(crate) fn stop(world: &mut World, request: &w::StopRequest) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    if let Some(agent) = agent::agent_of(world, &request.companion_id) {
        world.trigger(StopTurn::new(agent));
    }
    // Stop means stop speaking too; stale playback reports are discarded.
    let _discarded = super::voice::flush(
        world.get_resource::<super::voice::Voice>(),
        &request.companion_id,
        w::FlushReason::Cancelled,
    );
    Ok(())
}

/// The conversation transcript, oldest first, paged by message id.
pub(crate) fn history(
    world: &mut World,
    request: &w::HistoryRequestView<'_>,
) -> Result<w::History> {
    world
        .resource::<Store>()
        .companion_repo()
        .companion(request.companion_id)?
        .ok_or_else(|| HostError::NotFound("companion not found".into()))?;
    let after = request.cursor.and_then(|cursor| cursor.parse::<u64>().ok());
    let limit = request.limit.unwrap_or(100).clamp(1, 200) as usize;
    let mut items: Vec<_> = dialogue_entries(world, request.companion_id)?
        .rev()
        .filter(|entry| after.is_none_or(|after| entry.id.get() > after))
        .filter_map(|entry| dialogue_message(request.companion_id, &entry))
        .take(limit)
        .collect();
    items.reverse();
    Ok(w::History {
        items,
        observed_at: Some(world.resource::<super::notify::Notifier>().cursor()).into(),
        ..Default::default()
    })
}

pub(crate) fn dialogue_entries<'a>(
    world: &'a mut World,
    companion: &str,
) -> Result<impl DoubleEndedIterator<Item = EntryRef<'a>>> {
    let agent = agent::agent_of(world, companion)
        .ok_or_else(|| HostError::NotFound("companion agent not found".into()))?;
    Ok(world
        .get::<Conversation>(agent)
        .into_iter()
        .flat_map(|conversation| conversation.entries()))
}

/// One message by id.
pub(crate) fn message_by_id<'a>(
    entries: impl Iterator<Item = EntryRef<'a>>,
    request: &w::MessageRequestView<'_>,
) -> Result<w::Message> {
    let wanted = request
        .message_id
        .parse::<u64>()
        .map_err(|_| HostError::Invalid("message id is not a number".into()))?;
    entries
        .filter(|entry| entry.id.get() == wanted)
        .find_map(|entry| dialogue_message(request.companion_id, &entry))
        .ok_or_else(|| HostError::NotFound("message not found".into()))
}

fn dialogue_message(companion: &str, entry: &EntryRef<'_>) -> Option<w::Message> {
    agent::entry_text(entry).map(|text| message(companion, "", entry, text))
}

pub(crate) fn message(
    companion: &str,
    work: &str,
    entry: &EntryRef<'_>,
    text: String,
) -> w::Message {
    let role = agent::role_of(entry);
    let text = if work.is_empty() && role == w::MessageRole::Assistant {
        super::voice::speech::strip_motion_leak(&text)
    } else {
        text
    };
    w::Message {
        id: entry.id.get().to_string(),
        companion_id: companion.to_owned(),
        work_id: work.to_owned(),
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
            .map_or_default(rig_core::conversation::Author::label),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::Message;
    use rig_core::conversation::Entry;

    /// A conversation with an `ene.accepted` receipt, and the input it names
    /// when `input` gives the request id.
    fn world_with(receipt: serde_json::Value, input: Option<&str>) -> (World, Entity) {
        let mut conversation = Conversation::from(rig_core::conversation::Conversation::new());
        conversation.add_aside(Aside::new(
            "ene.accepted",
            Author::plugin("ene-host"),
            receipt,
        ));
        if let Some(request_id) = input {
            conversation.push(Entry::new(Message::user("hello")).by(Author {
                request: Some(RequestId(request_id.to_owned())),
                ..Author::user()
            }));
        }
        let mut world = World::new();
        let agent = world.spawn(conversation).id();
        (world, agent)
    }

    #[test]
    fn an_orphan_receipt_binds_no_content() {
        let receipt = json!({
            "request_key": "k1",
            "request_id": "r1",
            "hash": [1u8, 2, 3],
            "accepted_at": "2026-01-01T00:00:00Z",
        });
        // The input never reached the conversation, so the receipt is an
        // orphan: a retry with another message is not a conflict.
        let (world, agent) = world_with(receipt.clone(), None);
        assert_eq!(accepted(&world, agent, "k1", &[9, 9]).unwrap(), None);

        // Once the input is in the conversation the receipt binds its content.
        let (world, agent) = world_with(receipt, Some("r1"));
        assert!(accepted(&world, agent, "k1", &[9, 9]).is_err());
        assert_eq!(
            accepted(&world, agent, "k1", &[1, 2, 3])
                .unwrap()
                .as_deref(),
            Some("r1")
        );
        assert_eq!(accepted(&world, agent, "k2", &[1, 2, 3]).unwrap(), None);
    }
}
