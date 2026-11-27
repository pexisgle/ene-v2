//! Settling a run: the terminal state it records, the session it returns to
//! idle, the parent it reports to and the stall clock it releases.
use super::{Run, RunState, Runs, WorkStatus, inform, notify_finished, notify_status, retire};
use crate::host::{agent::WorkReports, db::Store, error::Failure, notify::Notifier};
use bevy_ecs::prelude::*;
use itertools::Itertools as _;
use rig_core::completion::{AssistantContent, Message};
use rig_ecs::prelude::*;

fn resolve_settle_outcome(
    reports: &WorkReports,
    run_id: &str,
    outcome: &TurnOutcome,
) -> (RunState, Option<String>) {
    let reported_failure = reports.failed.get(run_id).cloned();
    let reported_completed = reports.completed.get(run_id).cloned();
    match (reported_failure, reported_completed, outcome) {
        (Some(reason), _, _) => (RunState::Failed, Some(reason)),
        (None, Some(report), _) => (RunState::Completed, Some(report)),
        (None, None, TurnOutcome::Answered(message)) => {
            (RunState::Completed, message_text(message))
        }
        (None, None, TurnOutcome::Stopped(_)) => (RunState::Cancelled, None),
        (None, None, TurnOutcome::Failed(why)) => (
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    }
}

pub(crate) fn settle_run(world: &mut World, run_id: &str, outcome: &TurnOutcome) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.work_repo().run(run_id) else {
        return;
    };
    if run.state.is_terminal() {
        return;
    }
    let (state, text) = resolve_settle_outcome(world.resource::<WorkReports>(), run_id, outcome);
    finish_run(world, run, state, text);
}

pub(super) fn finish_run(world: &mut World, mut run: Run, state: RunState, text: Option<String>) {
    let store = world.resource::<Store>().clone();
    let active = match store.work_repo().active_run(&run.work_id) {
        Ok(active) => active,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            }
            return;
        }
    };
    let was_active = active.is_some_and(|active| active.run_id == run.run_id);
    let run_id = run.run_id.clone();
    run.state = state;
    run.pending_request_id = None;
    if state == RunState::Failed {
        run.failure = Some(Failure::new(
            "work_failed",
            text.clone().unwrap_or_else(|| "the run failed".to_owned()),
        ));
    }
    if let Err(error) = store.work_repo().update_run(&run) {
        crate::host::fail(world, &error.to_string());
        return;
    }
    crate::host::client::computer::invalidate_run(world, &run_id);
    let session = match store.work_repo().work_session(&run.work_id) {
        Ok(session) => session,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            }
            return;
        }
    };
    if let Some(session) = &session {
        {
            let mut notifier = world.resource_mut::<Notifier>();
            if was_active {
                notify_status(&mut notifier, session);
            }
            notify_finished(&mut notifier, session, &run);
        }
        if was_active {
            let raw_report = text.unwrap_or_else(|| {
                match state {
                    RunState::Cancelled => "作業を中断しました",
                    _ => "作業が終わりました",
                }
                .to_owned()
            });
            let report = if raw_report.starts_with('[') {
                raw_report
            } else {
                format!("[作業結果 (work_id: {})] {raw_report}", session.id)
            };
            inform(
                world,
                session.companion_id.as_str(),
                &run,
                report,
                DeliveryMode::Queue,
            );
        }
        if session.status == WorkStatus::Closed {
            retire(world, &run.work_id);
        } else if let Some(mut ready) = world.get_resource_mut::<super::ReadyWork>() {
            ready.0.insert(run.work_id.clone());
        }
    }
    world.resource_mut::<Runs>().forget(&run.run_id);
    let reports = &mut world.resource_mut::<WorkReports>();
    reports.completed.remove(&run.run_id);
    reports.failed.remove(&run.run_id);
}

pub(super) fn note_activity(runs: &mut Runs, run_id: &str) {
    runs.touch(run_id);
}

fn message_text(message: &Message) -> Option<String> {
    let Message::Assistant(reply) = message else {
        return None;
    };
    let text = reply
        .content
        .iter()
        .filter_map(AssistantContent::as_text)
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
