//! Dynamic context injected into one model request, never into the session log.
use crate::host::{
    agent::AgentKey,
    chat,
    companion::Companion,
    db::{Result, Store, now},
    error::HostError,
    memory::RecallMode,
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::Message,
    conversation::{Author, AuthorKind, Entry},
};
use rig_ecs::prelude::*;
use rig_memory::{HeuristicTokenCounter, TokenCounter};

const PINNED_BUDGET: usize = 1_000;
const REMEMBERED: usize = 3;

#[derive(Component)]
pub(crate) struct ContextInjected;

#[derive(Component, Clone)]
pub(crate) struct ContextSnapshot {
    text: String,
}

/// Builds the tail for one parent, user-authored turn.
pub(crate) fn build_context(
    store: &Store,
    companion_id: &str,
    input: &str,
    perception: Option<&str>,
) -> Result<Option<String>> {
    let companion = store
        .companion(companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let mut text = format!(
        "現在の心（revision {}）:\n{}",
        companion.state_revision,
        mind_text(&companion)
    );
    let pinned = store.pinned(companion_id, 16)?;
    let recalled = store.recall(companion_id, input, RecallMode::Automatic, REMEMBERED)?;
    let mut seen = std::collections::HashSet::new();
    let mut memories = Vec::new();
    let counter = HeuristicTokenCounter::default();
    let mut used = 0usize;
    for memory in pinned.into_iter().chain(recalled) {
        if !seen.insert(memory.id.clone()) {
            continue;
        }
        let tokens = counter.count(&Message::user(&memory.content));
        if !memories.is_empty() && used + tokens > PINNED_BUDGET {
            break;
        }
        used = used.saturating_add(tokens);
        memories.push(format!("- {}", memory.content));
    }
    if !memories.is_empty() {
        text.push_str("\n\n関連する記憶:\n");
        text.push_str(&memories.join("\n"));
    }
    if let Some(perception) = perception.filter(|text| !text.is_empty()) {
        text.push_str("\n\n知覚:\n");
        text.push_str(perception);
    }
    text.push_str(&format!("\n\n現在時刻: {}", now()));
    Ok(Some(text))
}

fn mind_text(companion: &Companion) -> String {
    if companion.state.is_empty() {
        "（まだ何もありません）".into()
    } else {
        serde_json::to_string(&companion.state).unwrap_or_default()
    }
}

/// Inserts the tail before the newest user entry of each admitted turn call.
#[allow(clippy::type_complexity)]
pub(crate) fn inject_context(
    mut commands: Commands,
    mut calls: Query<
        (Entity, &mut ModelRequest, &ForAgent, &CallOf),
        (With<TurnCall>, Added<Admitted>, Without<ContextInjected>),
    >,
    keys: Query<&AgentKey>,
    snapshots: Query<&ContextSnapshot>,
    store: Res<Store>,
    perception: Res<crate::host::resources::Perception>,
    mut notifier: ResMut<crate::host::notify::Notifier>,
) {
    for (call, mut request, agent, turn) in &mut calls {
        let Ok(key) = keys.get(agent.0) else {
            continue;
        };
        if key.is_work() {
            continue;
        }
        let Some(history) = request.history.as_mut() else {
            continue;
        };
        let Some((index, input)) = history.entries().rev().find_map(|entry| {
            if entry
                .author
                .is_some_and(|author| author.kind != AuthorKind::User)
            {
                return None;
            }
            let text = chat::entry_text(entry.message)?;
            (!text.is_empty()).then_some((entry.index, text))
        }) else {
            continue;
        };
        let snapshot = snapshots
            .get(turn.0)
            .ok()
            .map(|snapshot| snapshot.text.clone());
        let context = match snapshot {
            Some(text) => Ok(Some(text)),
            None => build_context(&store, key.companion(), &input, perception.0.as_deref()),
        };
        match context {
            Ok(Some(text)) => {
                commands
                    .entity(turn.0)
                    .insert(ContextSnapshot { text: text.clone() });
                history.splice(
                    index..index,
                    [Entry::new(Message::user(text))],
                    Author::plugin("ene.context"),
                );
                commands.entity(call).insert(ContextInjected);
            }
            Ok(None) => {}
            Err(error) if error.is_storage() => {
                let reason = error.to_string();
                commands.queue(move |world: &mut World| {
                    crate::host::fail(world, reason);
                });
            }
            Err(error) => {
                notifier.notice(key.companion(), "context_unavailable", &error.to_string());
                commands.entity(call).insert(ContextInjected);
            }
        }
    }
}
