//! View-only rolling compaction through rig-memory's CompactionPolicy.
//!
//! The on-disk SessionLog is append-only and is never rewritten. A completed
//! summary is stored on the agent and injected into the model request view at
//! `PrepareSystems::Edit`.
use super::{
    agent::AgentKey,
    db::Store,
    model,
    notify::Notifier,
    resources::Models,
    resources::{CompactionQueue, HostOptions, Inactivity},
};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::Message,
    conversation::{Aside, Author, Entry, MessageId},
};
use rig_ecs::prelude::*;
use rig_memory::{
    CompactReason, CompactionPolicy, HeuristicTokenCounter, Summarizer, SummaryState, TokenCounter,
    summary_author,
};
use serde_json::json;
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
