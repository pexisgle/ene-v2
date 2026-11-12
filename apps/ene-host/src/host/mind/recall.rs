//! Dynamic context injected into one model request, never into the session log.
use crate::host::{
    agent::AgentKey,
    chat,
    db::{Result, Store, now},
    error::HostError,
    mind::Companion,
    mind::RecallMode,
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::Message,
    conversation::{Author, AuthorKind, Entry, MessageId},
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
    /// The turn input the tail is inserted before; the same entry on every
    /// ReAct step keeps the prefix identical.
    before: MessageId,
}

/// Builds the tail for one parent turn (a user message or an agent report).
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

/// Inserts the tail before the turn's input entry, at the same position for
/// every ReAct step of the turn.
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
        let Some(history) = request.history.as_mut() else {
            continue;
        };
        let snapshot = snapshots.get(turn.0).ok().cloned();
        let (text, before) = match snapshot {
            Some(snapshot) => (snapshot.text, snapshot.before),
            None => {
                let Some((before, input)) = latest_input(history) else {
                    continue;
                };
                let context = if key.is_work() {
                    Ok(Some(format!("現在時刻: {}", now())))
                } else {
                    build_context(
                        &store,
                        key.companion(),
                        &input,
                        perception.0.get(key.companion()).map(String::as_str),
                    )
                };
                match context {
                    Ok(Some(text)) => {
                        commands.entity(turn.0).insert(ContextSnapshot {
                            text: text.clone(),
                            before,
                        });
                        (text, before)
                    }
                    Ok(None) => continue,
                    Err(error) if error.is_storage() => {
                        let reason = error.to_string();
                        commands.queue(move |world: &mut World| {
                            crate::host::fail(world, reason);
                        });
                        continue;
                    }
                    Err(error) => {
                        notifier.notice(key.companion(), "context_unavailable", &error.to_string());
                        commands.entity(call).insert(ContextInjected);
                        continue;
                    }
                }
            }
        };
        let index = context_index(history, before);
        history.splice(
            index..index,
            [Entry::new(Message::user(text))],
            Author::plugin("ene.context"),
        );
        commands.entity(call).insert(ContextInjected);
    }
}

/// Where the tail goes: before the turn input, or after a summary when a
/// mid-turn compaction already covered that input.
///
/// The fallback position stays the same for every later step of the turn, so
/// the injected tail does not move and the prefix cache keeps working.
fn context_index(history: &rig_core::conversation::Conversation, before: MessageId) -> usize {
    if let Some(index) = history
        .entries()
        .find_map(|entry| (entry.id == before).then_some(entry.index))
    {
        return index;
    }
    usize::from(
        history
            .entries()
            .next()
            .and_then(|entry| chat::entry_text(entry.message))
            .is_some_and(|text| text.starts_with("<summary>")),
    )
}

/// The entry that asked this turn: the user's own text or an agent's
/// instruction. Assistant replies and tool results carry no author (or rig's),
/// so they are never the tail's anchor.
fn latest_input(history: &rig_core::conversation::Conversation) -> Option<(MessageId, String)> {
    history.entries().rev().find_map(|entry| {
        let author = entry.author.as_ref()?;
        if !matches!(author.kind, AuthorKind::User | AuthorKind::Agent) {
            return None;
        }
        let text = chat::entry_text(entry.message)?;
        (!text.is_empty()).then_some((entry.id, text))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::conversation::Conversation;

    #[test]
    fn the_tail_stays_before_the_input_or_falls_back_after_a_summary() {
        let mut history = Conversation::from_messages([Message::user("話しかけた")]);
        let before = history.entries().next().unwrap().id;
        assert_eq!(context_index(&history, before), 0);

        history.splice(
            0..1,
            [Entry::new(Message::user(
                "<summary>\n古い会話の要約\n</summary>",
            ))],
            Author::plugin("ene.compaction"),
        );
        assert_eq!(
            context_index(&history, before),
            1,
            "a summary-covered input falls back to just after the summary"
        );
    }

    #[test]
    fn the_tail_starts_at_the_head_without_a_summary() {
        let history = Conversation::from_messages([Message::assistant("応答")]);
        let missing = MessageId::default();
        assert_eq!(context_index(&history, missing), 0);
    }
}
