//! A rolling summary of the conversation older than the model's window.
//!
//! The window in `kernel::fit` cuts the request's own history view, which is
//! never saved. This keeps what it cuts, so a long conversation keeps its
//! older context: the session log, `GetHistory` and the consolidation cursor
//! stay as they are. The summary lives on the agent it describes; a restart
//! asks for a new one at the first cut.
use crate::host::{Host, Owner, model};
use bevy_ecs::prelude::*;
use rig_core::{
    completion::Message,
    conversation::{Conversation, Entry},
};
use rig_ecs::prelude::*;
use rig_memory::{CompactReason, Summarizer, summary_author};
use std::sync::Arc;

/// Enough for a structured checkpoint; a longer reply is cut here.
const MAX_CHARS: usize = 8_000;
/// Below this the window has dropped too little to be worth a model call.
const MIN_DROPPED: usize = 2;

/// What the window cut, and on which agent.
#[derive(Component, Clone)]
pub(crate) struct Summary {
    /// The newest message it covers.
    pub(crate) through: u64,
    /// The text, empty when the last attempt failed.
    pub(crate) text: String,
}

impl Summary {
    pub(crate) fn text(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }
}

/// A summary being written for `agent`, up to `through`.
#[derive(Component, Clone)]
pub(crate) struct Job {
    pub(crate) agent: Entity,
    through: u64,
}

/// The marker that a summary call was handled.
#[derive(Component)]
pub(crate) struct Applied;

/// Puts the summary in front of what the window keeps.
///
/// It may not cover the newest of what was cut, which is what the next
/// summary is for; not sending it would lose the older part as well.
pub(crate) fn inject(history: &mut Conversation, summary: Option<&Summary>) {
    let Some(text) = summary.and_then(Summary::text) else {
        return;
    };
    let entry = Entry::new(Message::user(format!("<summary>\n{text}\n</summary>")));
    history.splice(0..0, [entry], summary_author());
}

/// Asks for a summary of what the window drops, unless one already covers it.
///
/// A failure, or no model to ask, is remembered at that point: the window
/// keeps working, and the host asks again only once the conversation has
/// grown past it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    commands: &mut Commands,
    host: &mut Host,
    agent: Entity,
    companion: &str,
    previous: Option<&Summary>,
    dropped: &[Arc<Message>],
    through: u64,
    pending: bool,
) {
    if pending || dropped.len() < MIN_DROPPED {
        return;
    }
    if previous.is_some_and(|summary| summary.through >= through) {
        return;
    }
    let models = host.models.clone();
    let Ok((connection, binding)) = models.connect(host, companion, model::Purpose::Memory) else {
        remember(commands, agent, through, String::new());
        return;
    };
    let Ok(request) = Summarizer::DEFAULT.request(
        dropped,
        previous.and_then(Summary::text).unwrap_or_default(),
        &CompactReason::Threshold,
        &connection.spec,
    ) else {
        remember(commands, agent, through, String::new());
        return;
    };
    commands.spawn((
        ModelRequest::new(request).on(connection),
        ForAgent(agent),
        Job { agent, through },
        binding,
    ));
}

#[allow(clippy::type_complexity)]
pub(crate) fn apply(
    mut calls: Query<(Entity, &Job, &Outcome), (ReadyToSettle, Without<Applied>)>,
    verdicts: Query<&RetryVerdict>,
    owners: Query<&Owner>,
    mut commands: Commands,
    mut host: ResMut<Host>,
) {
    for (call, job, outcome) in &mut calls {
        let (text, error) = match outcome.completion() {
            Some(response) => match Summarizer::summary_text(response) {
                Ok(text) => (text.chars().take(MAX_CHARS).collect(), None),
                Err(error) => (String::new(), Some(error.to_string())),
            },
            None => {
                let failed = verdicts
                    .get(call)
                    .is_ok_and(|verdict| matches!(verdict.0, Verdict::GaveUp(_) | Verdict::Final));
                if !failed {
                    continue;
                }
                (String::new(), Some("generation failed".into()))
            }
        };
        remember(&mut commands, job.agent, job.through, text);
        if let Some(error) = error
            && let Ok(owner) = owners.get(job.agent)
        {
            super::super::notice(&mut host, owner.companion(), "summary_failed", &error);
        }
        commands.entity(call).insert(Applied);
    }
}

fn remember(commands: &mut Commands, agent: Entity, through: u64, text: String) {
    commands.entity(agent).try_insert(Summary { through, text });
}
