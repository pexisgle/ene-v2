//! View-only rolling compaction through rig-memory's CompactionPolicy.
//!
//! The on-disk SessionLog is append-only and is never rewritten. A completed
//! summary is stored on the agent and injected into the model request view at
//! `PrepareSystems::Edit`.
use super::{
    agent::AgentKey,
    chat,
    db::Store,
    model::{self, Refusal},
    notify::Notifier,
    resources::Models,
    resources::{CompactionQueue, ConsolidationQueue, HostOptions, Inactivity},
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::Message,
    conversation::{Aside, Author, Entry, MessageId},
};
use rig_ecs::prelude::*;
use rig_memory::{
    ClearToolOutputs, CompactReason, CompactionPolicy, HeuristicTokenCounter, Summarizer,
    SummaryState, TokenCounter, recent, recent::TokenBudget, summary_author,
};
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Options {
    pub(crate) policy: CompactionPolicy,
    pub(crate) idle: Duration,
    pub(crate) threshold: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            policy: CompactionPolicy::default(),
            idle: Duration::from_secs(60),
            threshold: 0.70,
        }
    }
}

/// A summary being written for an agent, up to `keep_from`.
#[derive(Component, Clone)]
pub(crate) struct SummaryJob {
    pub(crate) keep_from: Option<MessageId>,
}

#[derive(Component)]
pub(crate) struct Applied;

/// Drops a running summary for `companion` because the user spoke, and
/// schedules a fresh attempt for the next idle period.
///
/// The task runs on the pool, so despawning its call is what cancels it: the
/// in-memory request view is discarded and no stale summary can be applied
/// after the live log has moved on.
pub(crate) fn interrupt(world: &mut World, companion: &str) {
    let jobs: Vec<Entity> = {
        let mut jobs =
            world.query_filtered::<(Entity, &ForAgent), (With<SummaryJob>, Without<Applied>)>();
        jobs.iter(world)
            .filter_map(|(call, agent)| {
                world
                    .get::<AgentKey>(agent.0)
                    .is_some_and(|key| !key.is_work() && key.companion() == companion)
                    .then_some(call)
            })
            .collect()
    };
    if jobs.is_empty() {
        return;
    }
    for call in jobs {
        world.despawn(call);
    }
    world
        .resource_mut::<CompactionQueue>()
        .0
        .insert(companion.to_owned());
}

/// The latest view-only summary for an agent.
#[derive(Component, Clone)]
pub(crate) struct Summary {
    pub(crate) text: String,
    pub(crate) through: u64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    mut commands: Commands,
    store: Res<Store>,
    models: Res<Models>,
    options: Res<HostOptions>,
    inactivity: Res<Inactivity>,
    time: Res<Time<Virtual>>,
    mut queue: ResMut<CompactionQueue>,
    agents: Query<(Entity, &AgentKey, &Conversation)>,
    summaries: Query<&Summary>,
    pending: Query<&ForAgent, (With<SummaryJob>, Without<Applied>)>,
) {
    if queue.0.is_empty() {
        return;
    }
    let now = time.elapsed();
    let pending_agents: std::collections::HashSet<Entity> =
        pending.iter().map(|agent| agent.0).collect();
    let queued: Vec<String> = queue.0.drain().collect();
    for companion_id in queued {
        let Some((agent, _, conversation)) = agents
            .iter()
            .find(|(_, key, _)| !key.is_work() && key.companion() == companion_id)
        else {
            continue;
        };
        if pending_agents.contains(&agent) {
            queue.0.insert(companion_id);
            continue;
        }
        let idle = inactivity
            .0
            .get(&companion_id)
            .is_none_or(|last| now.saturating_sub(*last) >= options.compaction.idle);
        if !idle {
            queue.0.insert(companion_id);
            continue;
        }
        let (connection, binding) =
            match models
                .0
                .connect(&store, &companion_id, model::Purpose::Compaction)
            {
                Ok(pair) => pair,
                Err(_) => continue,
            };
        let spec = connection.spec.clone();
        let Some(limit) = spec.input_limit() else {
            continue;
        };
        let counter = HeuristicTokenCounter::default();
        let used: usize = conversation
            .messages()
            .iter()
            .map(|message| counter.count(message))
            .sum();
        if (used as f64) / (limit as f64) < options.compaction.threshold {
            continue;
        }
        let mut view = conversation.clone();
        let mut state = SummaryState::default();
        if let Ok(summary) = summaries.get(agent) {
            state.summary = summary.text.clone();
            let covered = view
                .entries()
                .take_while(|entry| entry.id.get() <= summary.through)
                .count();
            if covered > 0 {
                view.splice(
                    0..covered,
                    [Entry::new(Message::user(format!(
                        "<summary>\n{}\n</summary>",
                        summary.text
                    )))],
                    summary_author(),
                );
            }
        }
        let Ok(plan) =
            options
                .compaction
                .policy
                .plan_summary(&view, &state, &CompactReason::Threshold, &spec)
        else {
            continue;
        };
        let Some(plan) = plan else {
            continue;
        };
        if let Some(keep) = plan.keep_from {
            let cursor = store
                .consolidation(&companion_id)
                .ok()
                .flatten()
                .map_or(0, |cursor| cursor.last_sequence);
            if options.consolidate && keep.get() > cursor {
                continue;
            }
        }
        commands.spawn((
            ModelRequest::new(plan.request).on(connection),
            ForAgent(agent),
            SummaryJob {
                keep_from: plan.keep_from,
            },
            binding,
        ));
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn apply(
    mut commands: Commands,
    mut jobs: Query<(Entity, &SummaryJob, &ForAgent, &Outcome), (ReadyToSettle, Without<Applied>)>,
    mut conversations: Query<&mut Conversation>,
    verdicts: Query<&RetryVerdict>,
    keys: Query<&AgentKey>,
    mut notifier: ResMut<Notifier>,
) {
    for (call, job, agent, outcome) in &mut jobs {
        let text = match outcome.completion() {
            Some(response) => match Summarizer::summary_text(response) {
                Ok(text) => Some(text.chars().take(8_000).collect::<String>()),
                Err(error) => {
                    notifier.notice(
                        keys.get(agent.0).map_or("companion", |key| key.companion()),
                        "summary_failed",
                        &error.to_string(),
                    );
                    None
                }
            },
            None => {
                let failed = verdicts
                    .get(call)
                    .is_ok_and(|verdict| matches!(verdict.0, Verdict::GaveUp(_) | Verdict::Final));
                if !failed {
                    continue;
                }
                notifier.notice(
                    keys.get(agent.0).map_or("companion", |key| key.companion()),
                    "summary_failed",
                    "generation failed",
                );
                None
            }
        };
        if let Some(text) = text {
            let through = job.keep_from.map_or(0, |id| id.get().saturating_sub(1));
            commands.entity(agent.0).insert(Summary {
                text: text.clone(),
                through,
            });
            if let Ok(mut conversation) = conversations.get_mut(agent.0) {
                conversation.add_aside(Aside::new(
                    "ene.summary",
                    Author::plugin("ene.compaction"),
                    json!({
                        "text": text,
                        "through_message_id": through,
                    }),
                ));
            }
        }
        commands.entity(call).insert(Applied);
    }
}

/// Restores the latest summary snapshot from the append-only log's aside.
pub(crate) fn restore(world: &mut World) {
    let agents: Vec<(Entity, AgentKey)> = world
        .query::<(Entity, &AgentKey, &Conversation)>()
        .iter(world)
        .filter(|(_, key, _)| !key.is_work())
        .map(|(entity, key, _)| (entity, key.clone()))
        .collect();
    for (entity, _) in agents {
        let Some(conversation) = world.get::<Conversation>(entity) else {
            continue;
        };
        let snapshot = conversation
            .asides()
            .rfind(|(_, aside)| aside.kind == "ene.summary")
            .and_then(|(_, aside)| {
                let text = aside.value.get("text")?.as_str()?.to_owned();
                let through = aside.value.get("through_message_id")?.as_u64()?;
                (!text.is_empty()).then_some(Summary { text, through })
            });
        if let Some(snapshot) = snapshot {
            world.entity_mut(entity).insert(snapshot);
        }
    }
}

/// Injects the latest summary at the head of a request's view only.
#[allow(clippy::type_complexity)]
pub(crate) fn inject(
    mut calls: Query<(&mut ModelRequest, &ForAgent), (With<TurnCall>, Added<Admitted>)>,
    summaries: Query<&Summary>,
) {
    for (mut call, agent) in &mut calls {
        let Ok(summary) = summaries.get(agent.0) else {
            continue;
        };
        if summary.text.is_empty() {
            continue;
        }
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let covered = history
            .entries()
            .take_while(|entry| entry.id.get() <= summary.through)
            .count();
        history.splice(
            0..covered,
            [Entry::new(Message::user(format!(
                "<summary>\n{}\n</summary>",
                summary.text
            )))],
            Author::plugin("ene.compaction"),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fit(
    mut calls: Query<(Entity, &mut ModelRequest, &ForAgent, &Adapted), Added<Admitted>>,
    keys: Query<&AgentKey>,
    store: Res<Store>,
    options: Res<HostOptions>,
    mut queues: ResMut<ConsolidationQueue>,
    mut compaction: ResMut<CompactionQueue>,
    mut notifier: ResMut<Notifier>,
    mut reported: Local<HashSet<(Entity, &'static str)>>,
    mut commands: Commands,
) {
    for (call_entity, mut call, agent, adapted) in &mut calls {
        let key = keys.get(agent.0).ok();
        let spec = &adapted.0.spec;
        if let Some(max) = spec.max_output_tokens {
            call.request_mut().max_tokens = Some(u64::from(max));
        }
        let Some(window) = spec.context_window else {
            if reported.insert((agent.0, "window"))
                && let Some(key) = key
            {
                notifier.notice(
                    key.companion(),
                    "context_window_unknown",
                    "the model's context length is unknown; the whole conversation is sent",
                );
            }
            continue;
        };
        let counter = HeuristicTokenCounter::default();
        let output = spec
            .max_output_tokens
            .map_or(0usize, |tokens| tokens as usize);
        // The docs reserve the output budget plus a 10% margin for its estimate.
        let reserve = output
            .saturating_add(output.div_ceil(10))
            .saturating_add(
                call.prompt
                    .as_ref()
                    .map_or(0, |prompt| counter.count(&Message::system(prompt.text()))),
            )
            .saturating_add(
                call.request
                    .tools
                    .iter()
                    .map(|tool| counter.count_tool(tool))
                    .sum::<usize>(),
            );
        let budget = (window as usize).saturating_sub(reserve);
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let protected = usize::from(
            history
                .entries()
                .next()
                .and_then(|entry| chat::entry_text(entry.message))
                .is_some_and(|text| text.starts_with("<summary>")),
        );
        let protected_tokens = history
            .messages()
            .first()
            .filter(|_| protected == 1)
            .map_or(0, |message| counter.count(message));
        let budget = budget.saturating_sub(protected_tokens);
        let Some(last) = history.messages().last() else {
            continue;
        };
        if counter.count(last) > budget {
            if let Some(key) = key {
                notifier.notice(
                    key.companion(),
                    "context_exceeded",
                    "the latest message does not fit the model's context window",
                );
            }
            commands
                .entity(call_entity)
                .insert(Refusal("context_exceeded".into()));
            continue;
        }
        let used: usize = history
            .messages()
            .iter()
            .map(|message| counter.count(message))
            .sum();
        if used > budget {
            ClearToolOutputs::new(budget.min(20_000))
                .clear_conversation(history, Author::plugin("ene.window"));
        }
        let kept = recent::view(&history.messages()[protected..], &TokenBudget::new(budget));
        let kept_len = kept.len();
        let due = compaction_due(kept, u64::from(window), options.compaction.threshold);
        let len = history.messages().len();
        let start = len
            .saturating_sub(kept_len)
            .max(protected)
            .min(len.saturating_sub(1));
        // Queue the summary when the view is near the threshold, whether or
        // not it had to drop messages: `compaction::start` re-checks idle
        // time and real usage before it runs.
        if due && let Some(key) = key {
            compaction.0.insert(key.companion().into());
        }
        if start <= protected {
            continue;
        }
        let Some(key) = key else {
            continue;
        };
        let cursor = store
            .consolidation(key.companion())
            .ok()
            .flatten()
            .unwrap_or_default();
        let dropped: Vec<(u64, bool)> = history
            .entries()
            .skip(protected)
            .take(start - protected)
            .map(|entry| (entry.id.get(), entry.author.is_none()))
            .collect();
        let unconsolidated = dropped
            .iter()
            .any(|(id, own)| *own && *id > cursor.last_sequence);
        history.splice(
            protected..start,
            [Entry::new(Message::user(WINDOW_NOTICE))],
            Author::plugin("ene.window"),
        );
        if unconsolidated {
            queues.0.insert(key.companion().into());
            if reported.insert((agent.0, "truncated")) {
                notifier.notice(
                    key.companion(),
                    "window_truncated_unconsolidated",
                    "the window dropped messages that are not consolidated into memory yet",
                );
            }
        }
    }
}

const WINDOW_NOTICE: &str = "[System Notice: 過去の一部ログを一時的に省略して対話を継続しています]";

fn compaction_due(kept: &[std::sync::Arc<Message>], window: u64, threshold: f64) -> bool {
    if window == 0 {
        return false;
    }
    let counter = HeuristicTokenCounter::default();
    let tokens: usize = kept.iter().map(|message| counter.count(message)).sum();
    (tokens as f64) / (window as f64) > threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn compaction_is_due_by_tokens_not_by_message_count() {
        // One long message used to divide its count (1) by the window and
        // never trigger compaction.
        let kept = [Arc::new(Message::user("x".repeat(4_000)))];
        assert!(compaction_due(&kept, 1_000, 0.70));
        let short = [Arc::new(Message::user("ok"))];
        assert!(!compaction_due(&short, 1_000, 0.70));
    }
}
