//! What the plugin hand's guest tells the host, applied once per frame.
//!
//! `collect` runs in `Update` after the agent loop. It applies the events of
//! each in-flight invocation, settles a run whose driver failed, answers the
//! approvals the user, head or card resolved, and turns parked input into the
//! session's next turn. The hand state it reads is `hand`'s; this module only
//! turns what arrived into host effects.

use super::hand::{Hands, Pending, dispatch_to_hand};
use super::{Run, RunState, WorkSession, WorkStatus};
use crate::host::{
    background,
    db::Store,
    permission::{self, ApprovalStatus},
    resources::HostRuntime,
    util::now,
};
use bevy_ecs::prelude::*;
use ene_plugin::{
    HarnessEvent, HarnessInput, PermissionOption, RequestPermissionOutcome, ToolCallUpdate,
};
use ene_work::harness::{arguments_of, select_permission_outcome, stop_outcome};
use rig_core::message::{CallId, ToolCall as RequestedCall, ToolFunction, ToolName};
use rig_ecs::prelude::*;
use tokio::sync::mpsc;

/// Applies what arrived since the last frame: the events of each in-flight
/// invocation, the outcome of a finished driver, the reply to a resolved
/// approval, and the input a finished run parked.
pub(crate) fn collect(world: &mut World) {
    let mut hands = std::mem::take(&mut *world.resource_mut::<Hands>());
    let batches = take_events(&mut hands);
    let store = world.resource::<Store>().clone();
    for (adapter, events) in batches {
        for event in events {
            apply(world, &mut hands, &store, &adapter, event);
        }
    }
    let driver_failures = take_driver_failures(&mut hands, &store);
    for (run, why) in driver_failures {
        super::finish_run(
            world,
            run,
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        );
    }
    let approvals = take_resolved_approvals(&mut hands, &store);
    if !approvals.is_empty() {
        let runtime = world.resource::<HostRuntime>().0.clone();
        let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
        for (adapter, request, reply) in approvals {
            dispatch_to_hand(
                &mut hands,
                &runtime,
                wake.as_ref(),
                &adapter,
                HarnessInput::Respond {
                    request_id: request,
                    outcome: reply,
                },
            );
        }
    }
    let parked = take_parked_sessions(&mut hands, &store);
    *world.resource_mut::<Hands>() = hands;
    for (session, text) in parked {
        let work_id = session.id.clone();
        if let Err(error) = super::start_run(world, &session, &text) {
            tracing::warn!(work = %work_id, "parked input could not start a run: {error}");
        }
    }
}

fn take_events(hands: &mut Hands) -> Vec<(String, Vec<ene_plugin::Event>)> {
    let adapters: Vec<String> = hands.by_adapter.keys().cloned().collect();
    let mut batches = Vec::new();
    for adapter in adapters {
        let Some(hand) = hands.by_adapter.get_mut(&adapter) else {
            continue;
        };
        let mut batch = Vec::new();
        let mut index = 0;
        while index < hand.events.len() {
            match hand.events[index].try_recv() {
                Ok(event) => batch.push(event),
                Err(mpsc::error::TryRecvError::Empty) => index += 1,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Drained and closed: the invocation that wrote it ended.
                    hand.events.remove(index);
                }
            }
        }
        if !batch.is_empty() {
            batches.push((adapter, batch));
        }
    }
    batches
}

fn apply(
    world: &mut World,
    hands: &mut Hands,
    store: &Store,
    adapter: &str,
    event: ene_plugin::Event,
) {
    let ene_plugin::Event::Harness { event } = event else {
        return;
    };
    let Some((work_id, companion_id)) = hands
        .by_adapter
        .get(adapter)
        .map(|hand| (hand.work_id.clone(), hand.companion_id.clone()))
    else {
        return;
    };
    let Ok(active) = store.work_repo().active_run(&work_id) else {
        return;
    };
    match event {
        HarnessEvent::Update { update } => {
            // A non-chunk update ends whatever message was accumulating, so
            // the head reads each report as one message, not fragments.
            let effects = {
                let Some(hand) = hands.by_adapter.get_mut(adapter) else {
                    return;
                };
                hand.buffer.apply_update(&update)
            };
            let Some(run) = active else {
                return;
            };
            if let Some(text) = effects.flushed {
                super::inform(
                    world,
                    &companion_id,
                    &run.run_id,
                    text,
                    DeliveryMode::Inform,
                );
            }
            if let Some(text) = effects.report {
                super::note_activity(&mut world.resource_mut(), &run.run_id);
                super::inform(
                    world,
                    &companion_id,
                    &run.run_id,
                    text,
                    DeliveryMode::Inform,
                );
            }
        }
        HarnessEvent::RequestPermission {
            request_id,
            tool_call,
            options,
        } => {
            if let Some(run) = active {
                request_permission(
                    world,
                    hands,
                    store,
                    PermissionTarget {
                        adapter,
                        run: &run,
                        companion: &companion_id,
                        request: &request_id,
                    },
                    tool_call,
                    options,
                );
            }
        }
        HarnessEvent::Stop { stop_reason } => {
            let text = {
                let Some(hand) = hands.by_adapter.get_mut(adapter) else {
                    return;
                };
                hand.buffer.take_message()
            };
            let Some(run) = active else {
                return;
            };
            if run.state.is_terminal() {
                // A stop that raced the user's own stop settles nothing.
                return;
            }
            let (state, report) = stop_outcome(stop_reason, text);
            super::finish_run(world, run, state, report);
        }
    }
}

struct PermissionTarget<'a> {
    adapter: &'a str,
    run: &'a Run,
    companion: &'a str,
    request: &'a str,
}

/// Parks the turn in an approval row and a held call, through the same
/// `Approvals` map and card the device gate uses: the user's card, the
/// `SubmitApproval` call and the head's `resolve_approval` all resolve it.
fn request_permission(
    world: &mut World,
    hands: &mut Hands,
    store: &Store,
    target: PermissionTarget<'_>,
    tool_call: ToolCallUpdate,
    options: Vec<PermissionOption>,
) {
    let PermissionTarget {
        adapter,
        run,
        companion,
        request,
    } = target;
    let (tool_name, arguments) = {
        let Some(hand) = hands.by_adapter.get_mut(adapter) else {
            return;
        };
        hand.buffer.clear_message();
        let tool_name = hand.buffer.resolve_tool_name(&tool_call);
        (tool_name, arguments_of(&tool_call))
    };
    let Ok(name) = ToolName::new(&tool_name) else {
        tracing::warn!(work = %run.work_id, "the harness named an empty tool");
        return;
    };
    let (normalized, hash) = permission::canonicalise(&tool_name, &arguments);
    let approval_id = uuid::Uuid::new_v4().to_string();
    let client_id = match store.work_repo().work_session(&run.work_id) {
        Ok(session) => session.and_then(|session| session.client_id),
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            return;
        }
    };
    let approval = permission::Approval {
        request_id: approval_id.clone(),
        session_id: companion.to_owned(),
        run_id: run.run_id.clone(),
        work_id: Some(run.work_id.clone()),
        action_payload_hash: hash,
        tool_name: tool_name.clone(),
        normalized_arguments: normalized,
        status: permission::ApprovalStatus::Pending,
        created_at: now(),
        resolved_at: None,
        client_id,
        capability: Some(tool_name.clone()),
    };
    let call = world
        .spawn(rig_ecs::tools::ToolRequest {
            call: RequestedCall::new(
                CallId::from_wire(tool_call.tool_call_id.0.as_ref()),
                ToolFunction::new(name, arguments),
            ),
        })
        .id();
    if let Err(error) = permission::issue_hold(world, store, approval.clone(), call) {
        world.entity_mut(call).despawn();
        crate::host::fail(world, error.to_string());
        return;
    }
    if let Some(hand) = hands.by_adapter.get_mut(adapter) {
        hand.pending = Some(Pending {
            approval: approval_id,
            call,
            request: request.to_owned(),
            options,
        });
    }
    let target = format!(
        "{tool_name} {}",
        crate::host::util::truncate(&approval.normalized_arguments, 200)
    );
    permission::notify_approval_card(
        &mut world.resource_mut::<crate::host::notify::Notifier>(),
        companion,
        &approval,
        &target,
    );
}

/// Collects finished drivers whose failure should settle the open run.
fn take_driver_failures(hands: &mut Hands, store: &Store) -> Vec<(Run, String)> {
    let adapters: Vec<String> = hands.by_adapter.keys().cloned().collect();
    let mut failures = Vec::new();
    for adapter in &adapters {
        let Some(hand) = hands.by_adapter.get_mut(adapter) else {
            continue;
        };
        if !hand
            .driver
            .as_ref()
            .is_some_and(tokio_util::task::AbortOnDropHandle::is_finished)
        {
            continue;
        }
        let Some(driver) = hand.driver.take() else {
            continue;
        };
        let failure = match background::take(driver) {
            Some(Ok(_)) => None,
            Some(Err(error)) => Some(error.to_string()),
            None => Some("the harness call ended without a result".to_owned()),
        };
        let Some(why) = failure else {
            continue;
        };
        let Ok(Some(run)) = store.work_repo().active_run(&hand.work_id) else {
            continue;
        };
        if !run.state.is_terminal() {
            failures.push((run, why));
        }
    }
    failures
}

/// Answers permission requests whose approval row the user, head, or card resolved.
fn take_resolved_approvals(
    hands: &mut Hands,
    store: &Store,
) -> Vec<(String, String, RequestPermissionOutcome)> {
    let resolved: Vec<(String, String, RequestPermissionOutcome)> = hands
        .by_adapter
        .iter()
        .filter_map(|(adapter, hand)| {
            let pending = hand.pending.as_ref()?;
            store.work_repo().active_run(&hand.work_id).ok().flatten()?;
            let Ok(Some(approval)) = store.approval_repo().approval(&pending.approval) else {
                return None;
            };
            let outcome = match approval.status {
                ApprovalStatus::Approved => select_permission_outcome(&pending.options, true),
                ApprovalStatus::Denied => select_permission_outcome(&pending.options, false),
                // The hold was released without a decision (a stop took
                // its call): answer so the guest's turn state does not
                // dangle.
                ApprovalStatus::Expired => RequestPermissionOutcome::Cancelled,
                ApprovalStatus::Pending => return None,
            };
            Some((adapter.clone(), pending.request.clone(), outcome))
        })
        .collect();
    for (adapter, _, _) in &resolved {
        if let Some(hand) = hands.by_adapter.get_mut(adapter) {
            hand.pending = None;
        }
    }
    resolved
}

/// Drains parked inputs for idle open sessions so `collect` can start their next turn.
fn take_parked_sessions(hands: &mut Hands, store: &Store) -> Vec<(WorkSession, String)> {
    hands
        .by_adapter
        .iter_mut()
        .filter_map(|(_, hand)| {
            if hand.parked.is_empty() {
                return None;
            }
            if store
                .work_repo()
                .active_run(&hand.work_id)
                .ok()
                .flatten()
                .is_some()
            {
                return None;
            }
            let Ok(Some(session)) = store.work_repo().work_session(&hand.work_id) else {
                return None;
            };
            if session.status == WorkStatus::Closed {
                return None;
            }
            let text = hand.parked.drain(..).collect::<Vec<_>>().join("\n\n");
            Some((session, text))
        })
        .collect()
}
