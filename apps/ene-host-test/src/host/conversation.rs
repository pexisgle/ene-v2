//! Settings, input acceptance and the conversation's history, snapshot and updates.
use super::{
    Binding, Host, Owner, PREAMBLE, Receipt, ReceiptStatus, model,
    store::{Result, storage},
    tick, tools,
};
use crate::proto::{chat::v1 as w, settings::v1::Settings};
use connectrpc::ConnectError;
use rig_ecs::prelude::*;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::broadcast;
use uuid::Uuid;

const MAX_INPUT: usize = 32 * 1024;

pub(super) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(ConnectError::invalid_argument("invalid identifier"));
    }
    Ok(())
}

pub(super) fn conversation(target: &w::Target) -> String {
    if target.work_id.is_empty() {
        format!("parent-{}", target.companion_id)
    } else {
        target.work_id.clone()
    }
}

pub(crate) fn settings(world: &World, companion: &str) -> Result<Settings> {
    validate_id(companion)?;
    Ok(world
        .resource::<Host>()
        .db
        .settings(companion)?
        .unwrap_or_else(|| Settings {
            endpoint: "https://api.openai.com/v1".into(),
            companion_model: "gpt-6-luna".into(),
            work_model: "gpt-6-luna".into(),
            ..Default::default()
        }))
}

pub(crate) fn set_settings(
    app: &mut App,
    companion: String,
    expected: u64,
    mut value: Settings,
) -> Result<Settings> {
    let old = settings(app.world(), &companion)?;
    if old.revision != expected {
        return Err(ConnectError::aborted("settings revision changed"));
    }
    model::validate(&value)?;
    value.endpoint = value.endpoint.trim_end_matches('/').into();
    value.revision = expected
        .checked_add(1)
        .ok_or_else(|| ConnectError::out_of_range("settings revision exhausted"))?;
    app.world()
        .resource::<Host>()
        .db
        .put_settings(&companion, &value)?;
    let entities: Vec<_> = app
        .world_mut()
        .query::<(Entity, &Owner)>()
        .iter(app.world())
        .filter_map(|(entity, owner)| (owner.companion() == companion).then_some(entity))
        .collect();
    for entity in entities {
        if let Some(binding) = app.world_mut().entity_mut(entity).take::<Binding>() {
            binding.permit.store(false, Ordering::Release);
        }
        app.world_mut().trigger(StopTurn {
            entity,
            reason: Some("settings changed".into()),
        });
        app.world_mut().entity_mut(entity).remove::<Connection>();
        changed(app.world_mut(), entity);
    }
    Ok(value)
}

pub(super) fn state(world: &World, companion: &str) -> Result<w::State> {
    world.resource::<Host>().db.state(companion)
}

pub(super) fn find(world: &mut World, id: &str) -> Option<Entity> {
    world
        .query::<(Entity, &AgentId)>()
        .iter(world)
        .find_map(|(entity, agent)| (agent.0 == id).then_some(entity))
}

pub(crate) fn target_entity(world: &mut World, target: &w::Target) -> Result<Entity> {
    validate_id(&target.companion_id)?;
    if !target.work_id.is_empty() {
        validate_id(&target.work_id)?;
        let work = world
            .resource::<Host>()
            .db
            .work(&target.work_id)?
            .ok_or_else(|| ConnectError::not_found("work not found"))?;
        if work.info.companion_id != target.companion_id {
            return Err(ConnectError::not_found("work not found"));
        }
    }
    let id = conversation(target);
    let entity = find(world, &id).unwrap_or_else(|| spawn_agent(world, target));
    attach_agent(world, entity, target);
    Ok(entity)
}

pub(super) fn spawn_agent(world: &mut World, target: &w::Target) -> Entity {
    let entity = world
        .spawn((
            Agent,
            AgentId(conversation(target)),
            SystemPrompt(PREAMBLE.into()),
        ))
        .id();
    attach_agent(world, entity, target);
    entity
}

pub(super) fn attach_agent(world: &mut World, entity: Entity, target: &w::Target) {
    if world.get::<Owner>(entity).is_none() {
        let owner = Owner(target.clone());
        let access = tools::access_for(world.resource::<tools::ToolCatalog>(), &owner);
        world.entity_mut(entity).insert((owner, access));
    }
}

pub(super) fn connect_model(world: &World, target: &w::Target) -> Result<(Connection, Binding)> {
    let config = settings(world, &target.companion_id)?;
    if !config.model_allowed {
        return Err(ConnectError::permission_denied(
            "model data transmission is not allowed",
        ));
    }
    let permit = Arc::new(AtomicBool::new(true));
    let connection =
        (world.resource::<Host>().factory)(&config, !target.work_id.is_empty(), permit.clone())?;
    Ok((
        connection,
        Binding {
            revision: config.revision,
            permit,
        },
    ))
}

pub(super) fn bind(world: &mut World, entity: Entity, target: &w::Target) -> Result<()> {
    let config = settings(world, &target.companion_id)?;
    if config.model_allowed
        && world
            .get::<Binding>(entity)
            .is_some_and(|binding| binding.revision == config.revision)
    {
        return Ok(());
    }
    let (connection, binding) = connect_model(world, target)?;
    world.entity_mut(entity).insert((connection, binding));
    Ok(())
}

pub(super) fn has_request(world: &mut World, entity: Entity, id: &str) -> bool {
    world
        .get::<Conversation>(entity)
        .is_some_and(|conversation| {
            conversation.entries().any(|entry| {
                entry
                    .author
                    .and_then(|author| author.request.as_ref())
                    .is_some_and(|request| request.0 == id)
            }) || conversation
                .asides()
                .any(|(_, aside)| aside.kind == "host.accepted" && aside.value.as_str() == Some(id))
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

pub(crate) fn accept(app: &mut App, request: w::SendMessageRequest) -> Result<w::Acceptance> {
    let target = request
        .target
        .into_option()
        .ok_or_else(|| ConnectError::invalid_argument("target is required"))?;
    validate_id(&request.request_key)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_INPUT {
        return Err(ConnectError::invalid_argument(
            "message must contain 1..32768 bytes",
        ));
    }
    let entity = target_entity(app.world_mut(), &target)?;
    let id = conversation(&target);
    let digest = format!("{:x}", Sha256::digest(request.text.as_bytes()));
    if let Some(receipt) = app
        .world()
        .resource::<Host>()
        .db
        .receipt(&id, &request.request_key)?
    {
        if receipt.digest != digest {
            return Err(ConnectError::already_exists(
                "request key was used with different content",
            ));
        }
        return match receipt.status {
            ReceiptStatus::Accepted => Ok(w::Acceptance {
                input_id: receipt.id,
                ..Default::default()
            }),
            ReceiptStatus::Pending | ReceiptStatus::NotDelivered => Err(ConnectError::unavailable(
                "previous acceptance was not confirmed; inspect state before sending a new key",
            )),
        };
    }
    bind(app.world_mut(), entity, &target)?;
    // Bound admission counts Rig's inbox, without keeping another queue.
    let pending = app
        .world_mut()
        .query_filtered::<&ForAgent, (With<Incoming>, With<Started>, Without<Outcome>)>()
        .iter(app.world())
        .filter(|agent| agent.0 == entity)
        .count();
    if pending >= 32 {
        return Err(ConnectError::resource_exhausted(
            "too many unanswered inputs",
        ));
    }
    let mut receipt = Receipt {
        id: Uuid::new_v4().to_string(),
        conversation: id.clone(),
        digest,
        status: ReceiptStatus::Pending,
    };
    app.world()
        .resource::<Host>()
        .db
        .put_receipt(&id, &request.request_key, &receipt)?;
    let author = Author {
        request: Some(RequestId(receipt.id.clone())),
        ..Author::user()
    };
    app.world_mut()
        .trigger(Deliver::new(entity, request.text, DeliveryMode::Steer).with_author(author));
    tick(app);
    if app.world().resource::<Host>().failed.is_some()
        || !has_request(app.world_mut(), entity, &receipt.id)
    {
        return Err(storage("input was not saved"));
    }
    receipt.status = ReceiptStatus::Accepted;
    app.world()
        .resource::<Host>()
        .db
        .put_receipt(&id, &request.request_key, &receipt)?;
    Ok(w::Acceptance {
        input_id: receipt.id,
        ..Default::default()
    })
}

/// Injects host context into a conversation without starting a turn.
///
/// A subsystem such as the mind adds here what the next model call should
/// know; `GetHistory` records the message with the source and role `context`.
#[allow(dead_code)] // The extension point: the mind is the first caller.
pub(crate) fn interject(
    world: &mut World,
    target: &w::Target,
    source: &str,
    text: String,
) -> Result<()> {
    let entity = target_entity(world, target)?;
    let author = Author::plugin(source);
    world.trigger(Deliver::new(entity, text, DeliveryMode::Inform).with_author(author));
    Ok(())
}

pub(crate) fn stop(app: &mut App, target: w::Target) -> Result<w::Empty> {
    let entity = target_entity(app.world_mut(), &target)?;
    app.world_mut().trigger(StopTurn {
        entity,
        reason: Some("user stopped".into()),
    });
    Ok(w::Empty::default())
}

pub(super) fn changed(world: &mut World, entity: Entity) {
    if let Some(Owner(target)) = world.get::<Owner>(entity).cloned() {
        world.resource_mut::<Host>().notify_changed(&target);
    }
}

pub(crate) fn history(world: &World, entity: Entity) -> w::History {
    let messages = world
        .get::<Conversation>(entity)
        .into_iter()
        .flat_map(|c| c.entries())
        .filter_map(|entry| {
            let (role, text) = match entry.message {
                message::Message::System { .. } => return None,
                message::Message::User { content } => {
                    let text = content
                        .iter()
                        .filter_map(|part| match part {
                            UserContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let role = match entry.author.map(|author| &author.kind) {
                        Some(AuthorKind::Agent) => "agent",
                        Some(AuthorKind::Plugin(_)) => "context",
                        _ => "user",
                    };
                    (role, text)
                }
                message::Message::Assistant(assistant) => ("assistant", assistant.text()),
            };
            (!text.is_empty()).then(|| w::HistoryMessage {
                id: entry.id.get(),
                role: role.into(),
                text,
                made: entry.made,
                input_id: entry
                    .author
                    .and_then(|a| a.request.as_ref())
                    .map_or_else(String::new, |r| r.0.clone()),
                author: entry.author.map_or_else(String::new, |a| a.label()),
                ..Default::default()
            })
        })
        .collect();
    w::History {
        messages,
        ..Default::default()
    }
}

pub(super) fn runs(world: &World, entity: Entity, id: &str) -> Vec<w::RunInfo> {
    let Some(c) = world.get::<Conversation>(entity) else {
        return Vec::new();
    };
    c.turns()
        .map(|(_, turn)| {
            let (state, failure) = turn_status(turn.end.as_ref());
            w::RunInfo {
                conversation_id: id.into(),
                run_id: format!("{id}:{}", turn.start.get()),
                input_id: turn_input(c, turn.start),
                state: state.into(),
                failure: failure.into(),
                ..Default::default()
            }
        })
        .collect()
}

pub(super) fn turn_status(end: Option<&TurnEnd>) -> (&'static str, &str) {
    match end {
        None => ("running", ""),
        Some(TurnEnd::Answered { .. }) => ("answered", ""),
        Some(TurnEnd::Stopped { why, .. }) => ("stopped", why.as_deref().unwrap_or_default()),
        Some(TurnEnd::Failed { why }) => ("failed", why),
    }
}

pub(super) fn turn_input(conversation: &Conversation, start: MessageId) -> String {
    conversation
        .request(start)
        .and_then(|entry| entry.author)
        .and_then(|author| author.request.as_ref())
        .map(|request| request.0.clone())
        .or_else(|| {
            conversation.asides().find_map(|(_, aside)| {
                (aside.kind == "host.turn_input"
                    && aside.value["start"].as_u64() == Some(start.get()))
                .then(|| aside.value["input_id"].as_str().map(str::to_owned))
                .flatten()
            })
        })
        .unwrap_or_default()
}

pub(crate) fn snapshot(app: &mut App, target: &w::Target) -> Result<w::Snapshot> {
    // shortcut: snapshots read the whole history; add pagination before sustained use.
    let entity = target_entity(app.world_mut(), target)?;
    let host = app.world().resource::<Host>();
    let id = conversation(target);
    Ok(w::Snapshot {
        epoch: host.epoch.clone(),
        sequence: host.sequence,
        conversation_id: id.clone(),
        history: history(app.world(), entity).into(),
        runs: runs(app.world(), entity, &id),
        works: host.db.works_for(&target.companion_id)?,
        requests: host.db.questions_for(&target.companion_id)?,
        state: state(app.world(), &target.companion_id)?.into(),
        ..Default::default()
    })
}

pub(crate) fn subscribe(
    app: &mut App,
    target: w::Target,
) -> Result<(w::Snapshot, broadcast::Receiver<w::Event>)> {
    // Both operations run on the owner thread; no update can fit between them.
    let receiver = app.world().resource::<Host>().events.subscribe();
    Ok((snapshot(app, &target)?, receiver))
}
