//! What the plugin hand's guest tells the host, applied once per frame.
//!
//! `collect` runs in `Update` after the agent loop. It applies the events of
//! each in-flight invocation, settles a run whose driver failed, answers the
//! approvals the user, head or card resolved. The hand state it reads is
//! `hand`'s; this module only
//! turns what arrived into host effects.

use super::hand::{Hand, Hands, Pending, Turn, dispatch};
use super::{Run, RunState};
use crate::host::{
    db::Store,
    permission::{self, ApprovalStatus},
    resources::HostRuntime,
};
use bevy_ecs::prelude::*;
use ene_plugin::{
    HarnessEvent, HarnessInput, PermissionOption, RequestPermissionOutcome, ToolCallUpdate,
};
use ene_store::util::now;
use ene_work::harness::{arguments_of, prompt_of, select_permission_outcome, stop_outcome};
use rig_core::message::{CallId, ToolCall as RequestedCall, ToolFunction, ToolName};
use rig_ecs::prelude::*;
use tokio::sync::oneshot;

/// Applies what arrived since the last frame: the events of each in-flight
/// invocation, the outcome of a finished driver, the reply to a resolved
/// approval.
pub(crate) fn collect(world: &mut World) {
    let mut hands = std::mem::take(&mut *world.resource_mut::<Hands>());
    if hands.by_adapter.is_empty() {
        return;
    }
    let store = world.resource::<Store>().clone();
    let revoked: Vec<_> = hands
        .by_adapter
        .values()
        .filter(|hand| hand.running.revoked())
        .map(|hand| hand.work_id.clone())
        .collect();
    for work in revoked {
        if let Some(call) = super::hand::release(&mut hands, &work) {
            let _removed = world.try_despawn(call);
        }
        match store.work_repo().active_run(&work) {
            Ok(Some(run)) => super::finish_run(
                world,
                run,
                RunState::Failed,
                Some("ハーネスが無効になったため、作業を中止しました".into()),
            ),
            Ok(None) => {}
            Err(error) => crate::host::fail(world, &error.to_string()),
        }
    }
    let runtime = world.resource::<HostRuntime>().0.clone();
    let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
    for hand in hands.by_adapter.values_mut() {
        let next = collect_hand(world, &store, hand);
        if let Some(input) = next {
            dispatch(hand, &runtime, wake.clone(), input);
        }
    }
    *world.resource_mut::<Hands>() = hands;
}

fn collect_hand(world: &mut World, store: &Store, hand: &mut Hand) -> Option<HarnessInput> {
    let mut active = match store.work_repo().active_run(&hand.work_id) {
        Ok(run) => run,
        Err(error) => {
            crate::host::fail(world, &error.to_string());
            return None;
        }
    };
    if let Some(mut invocation) = hand.invocation.take() {
        // The relay wakes the actor on each admission. Bound each frame even
        // if the producer refills immediately.
        for _ in 0..16 {
            let Ok(event) = invocation.events.try_recv() else {
                break;
            };
            if matches!(&hand.turn, Turn::Run(id) if active.as_ref().is_some_and(|run| run.run_id == *id))
                && let ene_plugin::Event::Harness { event } = event
            {
                if matches!(event, HarnessEvent::Update { .. }) {
                    apply(world, store, hand, &mut active, event);
                } else {
                    // A guest can emit its terminal event and then trap.
                    // Commit the resulting hold or stop only on success.
                    invocation.terminal = Some(event);
                }
            }
        }
        let outcome = if invocation.events.is_closed() && invocation.events.is_empty() {
            invocation.outcome.try_recv()
        } else {
            Err(oneshot::error::TryRecvError::Empty)
        };
        match outcome {
            Err(oneshot::error::TryRecvError::Empty) => {
                hand.invocation = Some(invocation);
                return None;
            }
            Ok(Ok(())) => {
                if let Some(event) = invocation.terminal.take() {
                    apply(world, store, hand, &mut active, event);
                }
            }
            failed => {
                if matches!(&hand.turn, Turn::Run(id) if active.as_ref().is_some_and(|run| run.run_id == *id))
                {
                    let why = match failed {
                        Ok(Err(error)) => error.to_string(),
                        _ => "the harness call ended without a result".to_owned(),
                    };
                    if let Some(run) = active.take() {
                        super::finish_run(world, run, RunState::Failed, Some(why));
                    }
                }
                hand.running.discard_harness_session();
                if let Some(pending) = hand.pending.take() {
                    let _removed = world.try_despawn(pending.call);
                }
                hand.turn = Turn::New;
            }
        }
    }
    next_input(hand, store, active)
}

fn next_input(hand: &mut Hand, store: &Store, active: Option<Run>) -> Option<HarnessInput> {
    if matches!(hand.turn, Turn::Cancel) {
        hand.turn = Turn::Ready;
        return Some(HarnessInput::Cancel);
    }
    let run = active?;
    if matches!(&hand.turn, Turn::Run(id) if *id == run.run_id) {
        return reply_to_approval(hand, store);
    }
    let prompt = prompt_of(&run.instruction);
    let input = if matches!(hand.turn, Turn::New) {
        HarnessInput::Start {
            prompt,
            cwd: std::env::current_dir().map_or_default(|path| path.display().to_string()),
        }
    } else {
        HarnessInput::Prompt { prompt }
    };
    hand.turn = Turn::Run(run.run_id);
    Some(input)
}

fn apply(
    world: &mut World,
    store: &Store,
    hand: &mut Hand,
    active: &mut Option<Run>,
    event: HarnessEvent,
) {
    let Some(run) = active.as_ref() else {
        return;
    };
    match event {
        HarnessEvent::Update { update } => {
            let effects = hand.buffer.apply_update(&update);
            if let Some(text) = effects.flushed {
                super::inform(world, &hand.companion_id, run, text, DeliveryMode::Inform);
            }
            if let Some(text) = effects.report {
                super::note_activity(&mut world.resource_mut(), &run.run_id);
                super::inform(world, &hand.companion_id, run, text, DeliveryMode::Inform);
            }
        }
        HarnessEvent::RequestPermission {
            request_id,
            tool_call,
            options,
        } => {
            request_permission(world, hand, store, run, (request_id, tool_call, options));
        }
        HarnessEvent::Stop { stop_reason } => {
            let text = hand.buffer.take_message();
            if let Some(run) = active.take() {
                let (state, report) = stop_outcome(stop_reason, text);
                super::finish_run(world, run, state, report);
            }
        }
    }
}

/// Parks the turn in an approval row and a held call, through the same
/// `Approvals` map and card the device gate uses: the user's card, the
/// `SubmitApproval` call and the head's `resolve_approval` all resolve it.
fn request_permission(
    world: &mut World,
    hand: &mut Hand,
    store: &Store,
    run: &Run,
    (request, tool_call, options): (String, ToolCallUpdate, Vec<PermissionOption>),
) {
    if hand.pending.is_some() {
        return;
    }
    let companion = &hand.companion_id;
    hand.buffer.clear_message();
    let tool_name = hand.buffer.resolve_tool_name(&tool_call);
    let arguments = arguments_of(&tool_call);
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
                crate::host::fail(world, &error.to_string());
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
        external_destination: None,
        search_summary: None,
    };
    let call = world
        .spawn(rig_ecs::tools::ToolRequest {
            call: RequestedCall::new(
                CallId::from_wire(tool_call.tool_call_id.0.as_ref()),
                ToolFunction::new(name, arguments),
            ),
        })
        .id();
    if let Err(error) = permission::issue_hold(world, store, &approval, call) {
        world.entity_mut(call).despawn();
        crate::host::fail(world, &error.to_string());
        return;
    }
    hand.pending = Some(Pending {
        approval: approval_id,
        call,
        request,
        options,
    });
    let target = format!(
        "{tool_name} {}",
        ene_store::util::truncate(&approval.normalized_arguments, 200)
    );
    permission::notify_approval_card(
        &mut world.resource_mut::<crate::host::notify::Notifier>(),
        companion,
        &approval,
        &target,
    );
}

fn reply_to_approval(hand: &mut Hand, store: &Store) -> Option<HarnessInput> {
    let pending = hand.pending.as_ref()?;
    let approval = store.approval_repo().approval(&pending.approval).ok()??;
    let outcome = match approval.status {
        ApprovalStatus::Approved => select_permission_outcome(&pending.options, true),
        ApprovalStatus::Denied => select_permission_outcome(&pending.options, false),
        ApprovalStatus::Expired => RequestPermissionOutcome::Cancelled,
        ApprovalStatus::Evaluating | ApprovalStatus::Pending => return None,
    };
    let pending = hand.pending.take()?;
    Some(HarnessInput::Respond {
        request_id: pending.request,
        outcome,
    })
}
