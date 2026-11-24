//! Turn lifecycle events.
//!
//! A turn is announced as streaming when it appears and published once when it
//! settles, including the eye report a watched turn carries and the run settle
//! a work agent's turn triggers.

use super::companion_of;
use super::transcript::{entry_text, role_of};
use crate::host::{
    notify::Notifier,
    resources::{CompanionAgent, WorkAgent},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

/// The agent rows a turn projection needs.
type TurnAgentRows = (
    &'static AgentId,
    Option<&'static CompanionAgent>,
    Option<&'static WorkAgent>,
);

/// Reports a companion turn that appeared this frame as streaming.
pub(super) fn turn_started(
    turns: Query<(&Turn, &TurnOf), Added<TurnOf>>,
    agents: Query<&CompanionAgent>,
    mut notifier: ResMut<Notifier>,
) {
    for (turn, of) in &turns {
        let Ok(agent) = agents.get(of.0) else {
            continue;
        };
        notifier.push(
            Some(agent.companion_id.as_str()),
            None,
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: agent.companion_id.clone(),
                turn_id: turn.start.get().to_string(),
                status: w::TurnStatus::Streaming.into(),
                failure: None.into(),
                ..Default::default()
            })),
        );
    }
}

type ToolActivity = Or<(Added<ToolRequest>, Added<Settled>)>;

pub(super) fn log_tools(
    tools: Query<(&ToolRequest, &ForAgent, Has<Settled>), ToolActivity>,
    agents: Query<TurnAgentRows>,
    mut notifier: ResMut<Notifier>,
) {
    for (request, target, settled) in &tools {
        let Ok((_, companion, work)) = agents.get(target.0) else {
            continue;
        };
        let Some(companion_id) = companion_of(companion, work) else {
            continue;
        };
        notifier.diagnostic(
            companion_id,
            if work.is_some() { "hands" } else { "head" },
            if settled {
                "tool_settled"
            } else {
                "tool_requested"
            },
            serde_json::json!({
                "tool": request.call.function.name.as_str(),
                "call_id": request.call.id.to_string(),
                "work_id": work.map(|work| work.work_id.as_str()),
                "run_id": work.map(|work| work.run_id.as_str()),
            }),
        );
    }
}

#[derive(bevy_ecs::system::SystemParam)]
pub(super) struct TurnSettledParams<'w, 's> {
    outcomes: Query<'w, 's, (&'static TurnOutcome, &'static ForAgent)>,
    turns: Query<
        'w,
        's,
        (
            &'static Turn,
            Option<&'static crate::host::eyes::ReportOfTurn>,
        ),
    >,
    agents: Query<'w, 's, TurnAgentRows>,
    conversations: Query<'w, 's, &'static Conversation>,
    notifier: ResMut<'w, Notifier>,
}

/// Publishes the final turn event once a companion turn settles, and settles
/// the run when a work agent's turn finishes.
pub(super) fn turn_settled(
    settled: On<Add<Settled>>,
    mut commands: Commands,
    mut params: TurnSettledParams<'_, '_>,
) {
    let turn_entity = settled.entity;
    let Ok((outcome, of)) = params.outcomes.get(turn_entity) else {
        return;
    };
    let Ok((id, companion, work)) = params.agents.get(of.0) else {
        return;
    };
    if let Some(companion) = companion {
        let companion_id = companion.companion_id.clone();
        let turn = params.turns.get(turn_entity).ok();
        let turn_id = turn
            .map(|(turn, _)| turn.start.get().to_string())
            .unwrap_or_else(|| id.0.clone());
        let (status, failure) = match outcome {
            TurnOutcome::Answered(_) => (w::TurnStatus::Answered, None),
            TurnOutcome::Stopped(_) => (w::TurnStatus::Stopped, None),
            TurnOutcome::Failed(why) => (
                w::TurnStatus::Failed,
                Some(w::Failure {
                    code: "turn_failed".into(),
                    message: crate::host::util::truncate(why, 500),
                    ..Default::default()
                }),
            ),
        };
        let eye_report = turn.and_then(|(turn, report)| {
            Some(eye_report_result(
                turn,
                &report?.id.0,
                outcome,
                params.conversations.get(of.0).ok(),
            ))
        });
        params.notifier.push(
            Some(companion_id.as_str()),
            None,
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: companion_id.clone(),
                turn_id,
                status: status.into(),
                failure: failure.into(),
                eye_report: eye_report.into(),
                ..Default::default()
            })),
        );
        // A stopped or failed turn must not speak its flushed remainder.
        let answered = matches!(outcome, TurnOutcome::Answered(_));
        commands.queue(move |world: &mut World| {
            if answered {
                let voice = world.get_resource::<crate::host::voice::Voice>().cloned();
                let mut notifier = world.get_resource_mut::<Notifier>();
                crate::host::voice::on_turn_end(
                    voice.as_ref(),
                    notifier.as_deref_mut(),
                    &companion_id,
                );
            } else {
                let _discarded = crate::host::voice::flush(
                    world.get_resource::<crate::host::voice::Voice>(),
                    &companion_id,
                    w::FlushReason::Cancelled,
                );
            }
        });
    }
    if let Some(work) = work {
        let run_id = work.run_id.clone();
        let outcome = outcome.clone();
        commands.queue(move |world: &mut World| {
            crate::host::work::settle_run(world, run_id, outcome);
        });
    }
}

fn eye_report_result(
    turn: &Turn,
    id: &str,
    outcome: &TurnOutcome,
    conversation: Option<&Conversation>,
) -> w::EyeReportResult {
    use w::eye_report_result::Disposition;
    let watched = matches!(outcome, TurnOutcome::Stopped(Some(reason)) if reason == crate::host::eyes::WATCHED);
    let completed = watched || matches!(outcome, TurnOutcome::Answered(_));
    // A tool step can already have published text before the turn ends.
    let reply = conversation.filter(|_| completed).and_then(|conversation| {
        let end = conversation.turns().find_map(|(_, record)| {
            (record.start > turn.start).then(|| conversation.start_of(record.start))
        });
        conversation
            .entries()
            .take(end.unwrap_or(usize::MAX))
            .skip(conversation.start_of(turn.start))
            .filter(|entry| role_of(entry) == w::MessageRole::Assistant)
            .filter_map(|entry| entry_text(&entry))
            .map(|text| crate::host::voice::speech::strip_motion_leak(&text))
            .rfind(|text| !text.is_empty())
            .map(|text| crate::host::util::truncate(&text, 1500))
    });
    let disposition = match (reply.is_some(), watched, completed) {
        (true, _, _) => Disposition::Replied,
        (false, true, _) => Disposition::Watched,
        (false, false, true) => Disposition::NoReply,
        (false, false, false) => Disposition::Unspecified,
    };
    w::EyeReportResult {
        report_id: id.to_owned(),
        disposition: disposition.into(),
        reply,
        ..Default::default()
    }
}
