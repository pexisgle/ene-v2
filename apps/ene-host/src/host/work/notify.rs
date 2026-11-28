//! What a work session tells the outside world: the status and run events a
//! companion's client receives, and the reports a run hands its parent.
use super::{Run, WorkSession};
use crate::host::{agent, notify::Notifier, wire};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

/// Hands one report from a work run to its parent, as the run's own words.
pub(super) fn inform(
    world: &mut World,
    companion: &str,
    run: &Run,
    text: String,
    mode: DeliveryMode,
) {
    let Some(parent) = agent::agent_of(world, companion) else {
        return;
    };
    let author = Author::agent(
        agent::work_agent_id(&run.work_id),
        Some(RequestId(run.run_id.clone())),
    );
    world.trigger(Deliver::new(parent, text, mode).with_author(author));
}

pub(super) fn notify_started(notifier: &Notifier, session: &WorkSession, run: &Run) {
    notify_event(
        notifier,
        session,
        &run.run_id,
        w::work_event::Detail::RunStarted(Box::default()),
    );
}

pub(super) fn notify_finished(notifier: &Notifier, session: &WorkSession, run: &Run) {
    notify_event(
        notifier,
        session,
        &run.run_id,
        w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(run))),
    );
}

pub(super) fn notify_status(notifier: &Notifier, session: &WorkSession) {
    notify_event(
        notifier,
        session,
        "",
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: w::WorkStatus::from(session.status).into(),
            ..Default::default()
        })),
    );
}

fn notify_event(
    notifier: &Notifier,
    session: &WorkSession,
    run_id: &str,
    detail: w::work_event::Detail,
) {
    notifier.push(
        Some(session.companion_id.as_str()),
        None,
        w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
            work_id: session.id.clone(),
            run_id: run_id.to_owned(),
            detail: Some(detail),
            ..Default::default()
        })),
    );
}
