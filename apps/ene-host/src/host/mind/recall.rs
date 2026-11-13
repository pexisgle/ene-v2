//! The dynamic tail and context fitting.
//!
//! Static persona lives in the system prompt. Everything that changes per turn
//! (time, mind, recalled memory, perception) is appended to the turn's own
//! request message, so the saved conversation is never polluted and the prefix
//! stays stable across the ReAct steps of one turn.
use crate::host::{
    agent::{self, CompanionAgent, WorkAgent},
    db::{Store, now},
};
use bevy_ecs::prelude::*;
use rig_core::completion::Message;
use rig_core::conversation::Author;
use rig_core::message::UserContent;
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// The tail rendered once per turn so every step of the turn sends the same
/// bytes.
#[derive(Resource, Default)]
pub(crate) struct TailCache {
    /// The agent is part of the key: turn numbers restart per agent.
    by_turn: HashMap<(Entity, u64), String>,
}

/// The request calls the host prepared in this pass.
type AdmittedCalls = (Added<Admitted>, With<TurnCall>);
/// A request call with the agent it serves.
type TurnRequest<'a> = (&'a mut ModelRequest, &'a CallOf, Option<&'a ForAgent>);
/// A request call with the model it was adapted to.
type AdaptedRequest<'a> = (&'a mut ModelRequest, &'a Adapted, &'a mut Decision);

/// Injects the dynamic tail into the request view only.
fn inject_tail(
    mut calls: Query<TurnRequest<'static>, AdmittedCalls>,
    turns: Query<&Turn>,
    agents: Query<(Option<&CompanionAgent>, Option<&WorkAgent>)>,
    conversations: Query<&Conversation>,
    store: Res<Store>,
    perception: Res<crate::host::resources::Perception>,
    mut cache: ResMut<TailCache>,
) {
    for (mut call, of, agent) in &mut calls {
        let Ok(turn) = turns.get(of.0) else {
            continue;
        };
        let Some(agent) = agent.map(|agent| agent.0) else {
            continue;
        };
        let turn_id = turn.start.get();
        let key = (agent, turn_id);
        let tail = match cache.by_turn.get(&key) {
            Some(tail) => tail.clone(),
            None => {
                // A cache that outlives its turns would grow without bound; the
                // settle hook prunes it, and this bound is the last resort.
                if cache.by_turn.len() > 256 {
                    cache.by_turn.clear();
                }
                let tail = render_tail(&store, &agents, &conversations, &perception, agent);
                cache.by_turn.insert(key, tail.clone());
                tail
            }
        };
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let at = history.start_of(turn.start);
        let author = Author::plugin("ene-host");
        history.edit_each(author, |entry| {
            (entry.index == at).then(|| append(entry.message, &tail))
        });
    }
}

fn append(message: &Message, tail: &str) -> Message {
    let Message::User { content } = message else {
        return message.clone();
    };
    let mut content = content.clone();
    content.push(UserContent::text(tail.to_owned()));
    Message::User { content }
}

fn render_tail(
    store: &Store,
    agents: &Query<(Option<&CompanionAgent>, Option<&WorkAgent>)>,
    conversations: &Query<&Conversation>,
    perception: &crate::host::resources::Perception,
    agent: Entity,
) -> String {
    let mut tail = String::from("---\n現在の動的コンテキスト（会話ログには保存されません）\n");
    tail.push_str(&format!("現在日時: {}\n", now()));
    let Some((companion, conversation)) = agent_view(agents, conversations, agent) else {
        return tail;
    };
    // Perception and the clock lead the tail so truncation cannot drop them:
    // the model must always know the current environment.
    if let Some(event) = perception.events.get(&companion) {
        tail.push_str(&format!("知覚: {}\n", event.summary));
    }
    if let Ok(Some(state)) = store.companion(&companion) {
        tail.push_str(&format!(
            "現在の心: 感情={} 関係={} 傾向={} 懸念={}\n",
            state.state.emotion.json,
            state.state.relationships.json,
            state.state.tendencies.json,
            state.state.concerns.json
        ));
        if let Ok(pinned) = store.pinned(&companion)
            && !pinned.is_empty()
        {
            tail.push_str("常に覚えていること:\n");
            for memory in pinned {
                tail.push_str("- ");
                tail.push_str(&memory.content);
                tail.push('\n');
            }
        }
        if let Some(query) = recent_user_text(conversation)
            && let Ok(recalled) =
                store.recall(&companion, &query, super::memory::RecallMode::Auto, 6)
            && !recalled.is_empty()
        {
            tail.push_str("今の話に関係する記憶:\n");
            for memory in recalled {
                tail.push_str("- ");
                tail.push_str(&memory.memory.content);
                tail.push('\n');
            }
        }
    }
    tail.push_str("---");
    crate::host::db::truncate(&tail, 6_000)
}

fn agent_view<'a>(
    agents: &'a Query<(Option<&CompanionAgent>, Option<&WorkAgent>)>,
    conversations: &'a Query<&Conversation>,
    agent: Entity,
) -> Option<(String, Option<&'a Conversation>)> {
    let (companion, work) = agents.get(agent).ok()?;
    let companion = companion
        .map(|agent| agent.companion_id.clone())
        .or_else(|| work.map(|agent| agent.companion_id.clone()))?;
    Some((companion, conversations.get(agent).ok()))
}

fn recent_user_text(conversation: Option<&Conversation>) -> Option<String> {
    let conversation = conversation?;
    conversation
        .entries()
        .rev()
        .find(|entry| entry.is_users_own())
        .and_then(|entry| agent::entry_text(&entry))
}

/// Keeps the request within the model's input limit. Oldest complete turns are
/// dropped first; a request that cannot fit at all is refused instead of sent.
fn fit(mut calls: Query<AdaptedRequest<'static>, AdmittedCalls>) {
    for (mut call, adapted, mut decision) in &mut calls {
        let limit = adapted
            .0
            .spec
            .input_limit()
            .map(u64::from)
            .unwrap_or(128_000);
        let reserve = u64::from(adapted.0.spec.max_output_tokens.unwrap_or(4_096)) + limit / 10;
        let budget = limit.saturating_sub(reserve);
        let Some(history) = call.history.as_ref() else {
            continue;
        };
        let mut tokens = estimate(history);
        if tokens <= budget {
            continue;
        }
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        while tokens > budget && history.messages().len() > 2 {
            let keep = history.messages().len().saturating_sub(2).max(1) / 2;
            let at = rig_core::conversation::pairing::safe_start(history.messages(), keep)
                .max(1)
                .min(history.messages().len());
            history.splice(..at, [], Author::plugin("ene-host"));
            tokens = estimate(history);
        }
        if tokens > budget {
            decision.decide(
                Decider::plugin("ene-host"),
                Outcome::refused("context_exceeded: the request does not fit the model window"),
            );
        }
    }
}

fn estimate(history: &rig_core::conversation::Conversation) -> u64 {
    // A conservative character-based estimate: the provider counts tokens
    // itself, and the host only needs a safe margin.
    let chars: usize = history
        .messages()
        .iter()
        .map(|message| match &**message {
            Message::System { content } => content.chars().count(),
            Message::User { content } => content
                .iter()
                .map(|item| match item {
                    UserContent::Text(text) => text.text.chars().count(),
                    _ => 64,
                })
                .sum(),
            Message::Assistant(reply) => reply
                .content
                .iter()
                .map(|item| match item {
                    rig_core::completion::AssistantContent::Text(text) => text.text.chars().count(),
                    _ => 64,
                })
                .sum(),
        })
        .sum();
    (chars / 3) as u64 + 512
}

/// The systems that manage the request view.
pub(crate) struct RecallPlugin;

impl Plugin for RecallPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TailCache>()
            .add_systems(AgentPass, inject_tail.in_set(PrepareSystems::Edit))
            .add_systems(AgentPass, fit.in_set(PrepareSystems::Fit))
            .add_systems(Update, prune_tail.after(AgentLoop));
    }
}

/// Drops a turn's cached tail when the turn settles.
fn prune_tail(
    mut settled: Query<(Option<&Turn>, Option<&ForAgent>), Added<Settled>>,
    mut cache: ResMut<TailCache>,
) {
    for (turn, agent) in &mut settled {
        if let (Some(turn), Some(agent)) = (turn, agent) {
            cache.by_turn.remove(&(agent.0, turn.start.get()));
        }
    }
}
