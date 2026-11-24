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
    run_id: &str,
    text: String,
    mode: DeliveryMode,
) {
    let Some(parent) = agent::agent_of(world, companion) else {
        return;
    };
    let author = Author::agent(agent::work_agent_id(run_id), None);
    world.trigger(Deliver::new(parent, text, mode).with_author(author));
}

pub(super) fn notify_finished(notifier: &mut Notifier, session: &WorkSession, run: &Run) {
    notify_run(
        notifier,
        session,
        run,
        w::WorkEventKind::RunFinished,
        w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(run))),
    );
}

pub(super) fn notify_status(notifier: &mut Notifier, session: &WorkSession) {
    notify_event(
        notifier,
        session,
        w::WorkEventKind::StatusChanged,
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: wire::work_status(session.status).into(),
            ..Default::default()
        })),
    );
}

fn notify_event(
    notifier: &mut Notifier,
    session: &WorkSession,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let event = wire::work_event(session, kind, detail);
    push_event(notifier, session.companion_id.as_str(), event);
}

pub(super) fn notify_run(
    notifier: &mut Notifier,
    session: &WorkSession,
    run: &Run,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let mut event = wire::work_event(session, kind, detail);
    event.run_id = run.run_id.clone();
    push_event(notifier, session.companion_id.as_str(), event);
}

fn push_event(notifier: &mut Notifier, companion: &str, event: w::WorkEvent) {
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::WorkEvent(Box::new(event)),
    );
}
