//! The plugin hand: a work session whose backend is a harness guest instead
//! of the in-process rig agent.
//!
//! The backend pick happens at `start_run`: a companion whose task
//! assignment names an ACP connection runs on the harness plugin the
//! connection's `endpoint.adapter` installs; every other assignment keeps
//! the rig agent. The host serves no application services to a harness guest
//! yet, so a guest that calls one is refused at the service boundary.
//!
//! Concurrency is bounded and honest: the runtime serves one instance per
//! (adapter, extension) and the guest keeps its per-session state in
//! instance memory, so ONE plugin-hand `WorkSession` per adapter is live at a
//! time. A second `StartWork` on the same connection while a session holds
//! the adapter is refused with an error naming the busy harness; `CloseWork`
//! releases the claim. A stop mid-invocation kills the guest call, so the
//! next request reloads a fresh instance and the hand's session starts over
//! with `Start`; a stop while the turn is parked on a permission request
//! delivers `Cancel` and keeps the session for the next `Prompt`. A host
//! restart loses the guest state entirely, so a restore settles an open
//! plugin-hand run as failed instead of pretending the turn survived.

use super::{Run, RunState, WorkSession, WorkStatus};
use crate::host::{
    background::{self, AbortOnDropHandle},
    db::{Result, Store, now},
    error::HostError,
    permission::{self, ApprovalStatus},
    providers::{Providers, Running},
    resources::HostRuntime,
};
use bevy_ecs::prelude::*;
use ene_plugin::{
    ContentBlock, Extension, HarnessEvent, HarnessInput, Operation, PermissionOption,
    PermissionOptionKind, PlanEntryStatus, RequestPermissionOutcome, SelectedPermissionOutcome,
    SessionUpdate, StopReason, ToolCallStatus, ToolCallUpdate, text_block,
};
use ene_plugin_runtime::instance::{DenyServices, Services};
use rig_core::message::{CallId, ToolCall as RequestedCall, ToolFunction, ToolName};
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::mpsc;

type DriverOutcome = std::result::Result<Value, HostError>;

/// The plugin hands this host is running, keyed by the adapter each holds.
/// One hand per adapter, for as long as the work session it serves is open.
#[derive(Resource, Default)]
pub(crate) struct Hands {
    by_adapter: HashMap<String, Hand>,
}

struct Hand {
    work_id: String,
    companion_id: String,
    /// The started harness instance. The runtime serves one per adapter, so
    /// holding it is the claim on the adapter.
    running: Arc<Running>,
    /// The invocation in flight, if any. One at a time: a turn spans several
    /// invocations (`Start`, `Respond`, `Prompt`) but never overlaps them.
    driver: Option<AbortOnDropHandle<DriverOutcome>>,
    /// The events channels that may still hold something to collect. A new
    /// invocation pushes its channel here instead of replacing one, because
    /// dropping a receiver a driver still writes to cancels that call and
    /// kills the guest; a drained, closed channel leaves on its own.
    events: Vec<mpsc::Receiver<ene_plugin::Event>>,
    /// The message a run of text chunks is accumulating, keyed by its id.
    message: Option<(Option<String>, String)>,
    /// Input accepted while a run was active; it becomes the next prompt.
    parked: VecDeque<String>,
    /// The approval the turn is parked on, if any.
    pending: Option<Pending>,
    /// Tool names by call id, so a permission card can name the tool.
    names: HashMap<String, String>,
}

struct Pending {
    /// The host approval row the request became.
    approval: String,
    /// The held tool-call entity; despawning it lets the permission module's
    /// orphan cleanup release the hold and expire the row.
    call: Entity,
    /// The request id the plugin is waiting on.
    request: String,
    /// The options the plugin offered; the reply is drawn from them.
    options: Vec<PermissionOption>,
}

/// The harness a companion's task assignment names, when it is an ACP
/// connection onto an installed harness plugin. Anything else keeps the
/// in-process rig agent.
pub(crate) fn selected(
    world: &World,
    companion: &crate::host::mind::Companion,
) -> Option<(String, Arc<Running>)> {
    let assignment = companion.model(crate::host::mind::Role::Task)?;
    let store = world.resource::<Store>();
    let settings = store.settings().ok()?;
    let connection = settings
        .connections
        .iter()
        .find(|connection| connection.id == assignment.connection_id)?;
    if connection.kind != crate::host::settings::ConnectionKind::Acp {
        return None;
    }
    let adapter = connection.endpoint().adapter.clone();
    let running = world
        .get_resource::<Providers>()?
        .running(&adapter, Extension::Harness)?;
    Some((adapter, running))
}

/// Starts a run on the plugin hand: claims the adapter for the session and
/// dispatches the first turn of the session as `Start`, or a follow-up as
/// `Prompt` on the session that already holds the adapter.
pub(crate) fn start_run(
    world: &mut World,
    session: &WorkSession,
    run: &Run,
    instruction: &str,
    adapter: String,
    running: Arc<Running>,
) -> Result<()> {
    let claimed = {
        let hands = world.resource::<Hands>();
        if let Some(owner) = hands.by_adapter.get(&adapter)
            && owner.work_id != session.id
        {
            return Err(HostError::Unavailable(format!(
                "the harness plugin `{adapter}` is already serving work session `{}`",
                owner.work_id
            )));
        }
        hands.by_adapter.contains_key(&adapter)
    };
    let input = if claimed {
        HarnessInput::Prompt {
            prompt: prompt_of(instruction),
        }
    } else {
        HarnessInput::Start {
            prompt: prompt_of(instruction),
            cwd: cwd(),
        }
    };
    world.resource_mut::<Hands>().by_adapter.insert(
        adapter.clone(),
        Hand {
            work_id: session.id.clone(),
            companion_id: session.companion_id.clone(),
            running,
            driver: None,
            events: Vec::new(),
            message: None,
            parked: VecDeque::new(),
            pending: None,
            names: HashMap::new(),
        },
    );
    super::note_activity(world, &run.run_id);
    dispatch(world, &adapter, input);
    Ok(())
}

/// Whether a work session runs on the plugin hand.
pub(crate) fn owns(world: &World, work_id: &str) -> bool {
    adapter_of(world, work_id).is_some()
}

/// `send_work` while the hand's run is active: the guest is inside its turn
/// and cannot take a second prompt mid-invocation, so the input is parked
/// and becomes the next prompt turn once the run settles.
pub(crate) fn steer(world: &mut World, work_id: &str, text: &str) {
    edit_hand(world, work_id, |hand| {
        hand.parked.push_back(text.to_owned());
    });
}

/// Stops the hand's turn. A run mid-invocation is killed outright, which also
/// ends the session (the next request reloads a fresh guest); a run parked on
/// a permission request is told `Cancel` and keeps its session.
pub(crate) fn stop(world: &mut World, work_id: &str) {
    let Some(adapter) = adapter_of(world, work_id) else {
        return;
    };
    let (running, in_flight, parked) = {
        let Some(hand) = world.resource::<Hands>().by_adapter.get(&adapter) else {
            return;
        };
        (
            Arc::clone(&hand.running),
            hand.driver
                .as_ref()
                .is_some_and(|driver| !driver.is_finished()),
            hand.pending.is_some(),
        )
    };
    if in_flight {
        edit_hand(world, work_id, |hand| {
            // Dropping the driver aborts the call, which leaves the instance
            // without its guest. The next request reloads one whose session
            // state is empty, so the hand's claim on the adapter ends here.
            hand.driver = None;
            hand.message = None;
            hand.parked.clear();
        });
        drop_pending(world, &adapter);
        running.note_cancelled();
        world.resource_mut::<Hands>().by_adapter.remove(&adapter);
        return;
    }
    // The turn is parked on a permission request (or already drained): tell
    // the guest its turn was cancelled, and let the approval row expire.
    if parked {
        edit_hand(world, work_id, |hand| {
            hand.message = None;
            hand.parked.clear();
        });
        drop_pending(world, &adapter);
        dispatch(world, &adapter, HarnessInput::Cancel);
    }
}

/// `CloseWork` drops the harness session: the hand releases its adapter claim
/// and the guest keeps whatever state it kept, as the concurrency bound above
/// documents. An invocation still in flight dies with the hand, so the next
/// request reloads a fresh guest.
pub(crate) fn release(world: &mut World, work_id: &str) {
    let Some(adapter) = adapter_of(world, work_id) else {
        return;
    };
    // A parked approval, if any, is a hold on a call of the session being
    // closed; dropping the call lets the orphan path expire the row.
    drop_pending(world, &adapter);
    let in_flight = world
        .resource::<Hands>()
        .by_adapter
        .get(&adapter)
        .is_some_and(|hand| {
            hand.driver
                .as_ref()
                .is_some_and(|driver| !driver.is_finished())
        });
    if in_flight {
        // Closing the session drops the hand, which drops the events channel
        // an in-flight call is still writing to. That call then fails and
        // leaves the instance without its guest, so the next request must
        // reload one: abort the driver here and note the cancellation, the
        // same way a stop does. Leaving it to the drop would strand the
        // adapter on an unusable instance.
        edit_hand(world, work_id, |hand| hand.driver = None);
    }
    if let Some(hand) = world.resource_mut::<Hands>().by_adapter.remove(&adapter)
        && in_flight
    {
        hand.running.note_cancelled();
    }
}

/// Restores nothing but settles honestly: a restart lost the guest's session
/// state, so an open run this backend would serve reads failed naming the
/// restart. Runs the rig agent serves keep their own restore path.
pub(crate) fn settle_restarts(world: &mut World, run: Run) {
    super::finish_run(
        world,
        run,
        RunState::Failed,
        Some(
            "作業を続けられませんでした: ホストの再起動でハーネスのセッション状態が失われました"
                .to_owned(),
        ),
    );
}

/// Takes the pending approval out of the hand and drops its held call, which
/// the permission module's orphan path answers by releasing the hold and
/// expiring the row.
fn drop_pending(world: &mut World, adapter: &str) {
    let call = world
        .resource_mut::<Hands>()
        .by_adapter
        .get_mut(adapter)
        .and_then(|hand| hand.pending.take())
        .map(|pending| pending.call);
    if let Some(call) = call
        && world.get_entity(call).is_ok()
    {
        world.entity_mut(call).despawn();
    }
}

fn adapter_of(world: &World, work_id: &str) -> Option<String> {
    world
        .resource::<Hands>()
        .by_adapter
        .iter()
        .find(|(_, hand)| hand.work_id == work_id)
        .map(|(adapter, _)| adapter.clone())
}

/// Runs `edit` on the hand that serves `work_id`, if one exists. The guard
/// the resource hands out cannot leave this function, so the edit happens
/// here instead of returning a reference.
fn edit_hand<R>(world: &mut World, work_id: &str, edit: impl FnOnce(&mut Hand) -> R) -> Option<R> {
    let mut hands = world.resource_mut::<Hands>();
    hands
        .by_adapter
        .values_mut()
        .find(|hand| hand.work_id == work_id)
        .map(edit)
}

fn prompt_of(instruction: &str) -> Vec<ContentBlock> {
    // The instruction is the whole prompt: skills are not implemented, so
    // there is no lead-in to attach.
    vec![text_block(instruction)]
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn dispatch(world: &mut World, adapter: &str, input: HarnessInput) {
    let running = Arc::clone(&world.resource::<Hands>().by_adapter[adapter].running);
    let services: Arc<dyn Services> = Arc::new(DenyServices);
    let (sender, events) = mpsc::channel(16);
    let runtime = world.resource::<HostRuntime>().0.clone();
    let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
    let driver = background::spawn(&runtime, wake.as_ref(), async move {
        running
            .invoke_streaming(Operation::Harness { input }, services, sender)
            .await
    });
    let mut hands = world.resource_mut::<Hands>();
    if let Some(hand) = hands.by_adapter.get_mut(adapter) {
        hand.driver = Some(driver);
        hand.events.push(events);
    }
}

// === The app-thread collector ==============================================

/// Applies what arrived since the last frame: the events of each in-flight
/// invocation, the outcome of a finished driver, the reply to a resolved
/// approval, and the input a finished run parked.
pub(crate) fn collect(world: &mut World) {
    let batches = take_events(world);
    for (adapter, events) in batches {
        for event in events {
            apply(world, &adapter, event);
        }
    }
    finish_drivers(world);
    resume_approvals(world);
    deliver_parked(world);
}

fn take_events(world: &mut World) -> Vec<(String, Vec<ene_plugin::Event>)> {
    let mut hands = world.resource_mut::<Hands>();
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

fn apply(world: &mut World, adapter: &str, event: ene_plugin::Event) {
    let ene_plugin::Event::Harness { event } = event else {
        return;
    };
    let Some((work_id, companion_id)) = world
        .resource::<Hands>()
        .by_adapter
        .get(adapter)
        .map(|hand| (hand.work_id.clone(), hand.companion_id.clone()))
    else {
        return;
    };
    let store = world.resource::<Store>().clone();
    let Ok(active) = store.active_run(&work_id) else {
        return;
    };
    match event {
        HarnessEvent::Update { update } => {
            // A non-chunk update ends whatever message was accumulating, so
            // the head reads each report as one message, not fragments.
            let (flushed, report) = {
                let mut hands = world.resource_mut::<Hands>();
                let Some(hand) = hands.by_adapter.get_mut(adapter) else {
                    return;
                };
                let chunk = matches!(update, SessionUpdate::AgentMessageChunk(_));
                let flushed = if chunk {
                    None
                } else {
                    hand.message.take().map(|(_, text)| text)
                };
                let report = progress(hand, &update);
                (flushed, report)
            };
            let Some(run) = active else {
                return;
            };
            if let Some(text) = flushed {
                super::inform(
                    world,
                    &companion_id,
                    &run.run_id,
                    text,
                    DeliveryMode::Inform,
                );
            }
            if let Some(text) = report {
                super::note_activity(world, &run.run_id);
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
                    adapter,
                    &run,
                    &companion_id,
                    &request_id,
                    tool_call,
                    options,
                );
            }
        }
        HarnessEvent::Stop { stop_reason } => {
            let text = {
                let mut hands = world.resource_mut::<Hands>();
                let Some(hand) = hands.by_adapter.get_mut(adapter) else {
                    return;
                };
                hand.message.take().map(|(_, text)| text)
            };
            let Some(run) = active else {
                return;
            };
            if run.state.is_terminal() {
                // A stop that raced the user's own stop settles nothing.
                return;
            }
            let (state, report) = match stop_reason {
                StopReason::EndTurn => (RunState::Completed, text),
                StopReason::Cancelled => (RunState::Cancelled, None),
                StopReason::Refusal => (RunState::Failed, Some("作業を拒否しました".to_owned())),
                _ => (
                    RunState::Failed,
                    Some("作業を続けられませんでした: ハーネスの上限に達しました".to_owned()),
                ),
            };
            super::finish_run(world, run, state, report);
        }
    }
}

/// The progress text one session update owes the head, and the tool name it
/// may have taught the hand.
fn progress(hand: &mut Hand, update: &SessionUpdate) -> Option<String> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let ene_plugin::ContentBlock::Text(text) = &chunk.content else {
                return None;
            };
            let id = chunk.message_id.as_ref().map(|id| id.to_string());
            let held = if hand
                .message
                .as_ref()
                .is_some_and(|(previous, _)| previous.as_deref() != id.as_deref())
            {
                hand.message.take().map(|(_, text)| text)
            } else {
                None
            };
            let (_, accumulated) = hand.message.get_or_insert_with(|| (id, String::new()));
            accumulated.push_str(&text.text);
            held.map(|text| format!("[作業報告] {text}"))
        }
        SessionUpdate::ToolCall(call) => {
            if let Some(name) = &call.name {
                hand.names
                    .insert(call.tool_call_id.to_string(), name.clone());
            }
            let name = call
                .name
                .clone()
                .unwrap_or_else(|| call.tool_call_id.to_string());
            Some(format!("[作業報告] ツール {name}: {}", call.title))
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if let Some(name) = &update.fields.name {
                hand.names
                    .insert(update.tool_call_id.to_string(), name.clone());
            }
            let name = update
                .fields
                .name
                .clone()
                .or_else(|| hand.names.get(&update.tool_call_id.to_string()).cloned())
                .unwrap_or_else(|| update.tool_call_id.to_string());
            let status = update
                .fields
                .status
                .map(|status| match status {
                    ToolCallStatus::Pending => "準備中",
                    ToolCallStatus::InProgress => "実行中",
                    ToolCallStatus::Completed => "完了",
                    ToolCallStatus::Failed => "失敗",
                    _ => "更新",
                })
                .unwrap_or("更新");
            Some(format!("[作業報告] ツール {name}: {status}"))
        }
        SessionUpdate::Plan(plan) => {
            let entries = plan
                .entries
                .iter()
                .map(|entry| {
                    let status = match entry.status {
                        PlanEntryStatus::Pending => "未着手",
                        PlanEntryStatus::InProgress => "進行中",
                        PlanEntryStatus::Completed => "完了",
                        _ => "保留",
                    };
                    format!("- {}（{status}）", entry.content)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!("[作業報告] 計画:\n{entries}"))
        }
        // The runtime rejects the other update kinds at the contract
        // boundary, so they never reach the collector.
        _ => None,
    }
}

/// Parks the turn in an approval row and a held call, through the same
/// `Approvals` map and card the device gate uses: the user's card, the
/// `SubmitApproval` call and the head's `resolve_approval` all resolve it.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_arguments)]
fn request_permission(
    world: &mut World,
    adapter: &str,
    run: &Run,
    companion: &str,
    request: &str,
    tool_call: ToolCallUpdate,
    options: Vec<PermissionOption>,
) {
    let (tool_name, arguments) = {
        let mut hands = world.resource_mut::<Hands>();
        let Some(hand) = hands.by_adapter.get_mut(adapter) else {
            return;
        };
        hand.message = None;
        let tool_name = tool_call
            .fields
            .name
            .clone()
            .or_else(|| hand.names.get(&tool_call.tool_call_id.to_string()).cloned())
            .unwrap_or_else(|| tool_call.tool_call_id.to_string());
        (tool_name, arguments_of(&tool_call))
    };
    let store = world.resource::<Store>().clone();
    let Ok(name) = ToolName::new(&tool_name) else {
        tracing::warn!(work = %run.work_id, "the harness named an empty tool");
        return;
    };
    let (normalized, hash) = permission::canonicalise(&tool_name, &arguments);
    let approval_id = uuid::Uuid::new_v4().to_string();
    let client_id = match store.work_session(&run.work_id) {
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
    if let Err(error) = permission::issue_hold(world, &store, approval.clone(), call) {
        world.entity_mut(call).despawn();
        crate::host::fail(world, error.to_string());
        return;
    }
    if let Some(hand) = world.resource_mut::<Hands>().by_adapter.get_mut(adapter) {
        hand.pending = Some(Pending {
            approval: approval_id,
            call,
            request: request.to_owned(),
            options,
        });
    }
    let target = format!(
        "{tool_name} {}",
        crate::host::db::truncate(&approval.normalized_arguments, 200)
    );
    permission::notify_approval_card(world, companion, &approval, &target);
}

/// The canonical document a permission card shows and the approval hash is
/// taken over: the fields the plugin's tool-call update carries.
fn arguments_of(update: &ToolCallUpdate) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(title) = &update.fields.title {
        object.insert("title".into(), json!(title));
    }
    if let Some(name) = &update.fields.name {
        object.insert("name".into(), json!(name));
    }
    if let Some(content) = &update.fields.content {
        let text = content
            .iter()
            .filter_map(|item| match item {
                ene_plugin::ToolCallContent::Content(content) => match &content.content {
                    ene_plugin::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        object.insert("content".into(), json!(text));
    }
    if let Some(locations) = &update.fields.locations {
        let paths = locations
            .iter()
            .map(|location| location.path.display().to_string())
            .collect::<Vec<_>>();
        object.insert("locations".into(), json!(paths));
    }
    if let Some(raw) = &update.fields.raw_input {
        object.insert("raw_input".into(), raw.clone());
    }
    if let Some(raw) = &update.fields.raw_output {
        object.insert("raw_output".into(), raw.clone());
    }
    Value::Object(object)
}

/// The outcome that answers a permission request the user approved or denied:
/// the option the plugin named `allow` or `deny`, else the first one-shot
/// option of that kind. A request that offers neither is answered with a
/// cancel, which keeps the guest's turn from dangling.
fn outcome(options: &[PermissionOption], allow: bool) -> RequestPermissionOutcome {
    let wanted = if allow { "allow" } else { "deny" };
    let kind = if allow {
        PermissionOptionKind::AllowOnce
    } else {
        PermissionOptionKind::RejectOnce
    };
    let selected = options
        .iter()
        .find(|option| option.option_id.to_string() == wanted)
        .or_else(|| options.iter().find(|option| option.kind == kind));
    selected.map_or(RequestPermissionOutcome::Cancelled, |option| {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
    })
}

/// Takes the driver that finished and settles the run it left open. The
/// events arrive before this runs, so an orderly driver has already settled
/// its run; one that failed without a `Stop` settles it failed.
fn finish_drivers(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let failures: Vec<(String, String)> = {
        let mut hands = world.resource_mut::<Hands>();
        let adapters: Vec<String> = hands.by_adapter.keys().cloned().collect();
        let mut failures = Vec::new();
        for adapter in &adapters {
            let Some(hand) = hands.by_adapter.get_mut(adapter) else {
                continue;
            };
            if !hand
                .driver
                .as_ref()
                .is_some_and(|driver| driver.is_finished())
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
            if let Some(why) = failure {
                failures.push((adapter.clone(), why));
            }
        }
        failures
    };
    for (adapter, why) in failures {
        let work_id = world
            .resource::<Hands>()
            .by_adapter
            .get(&adapter)
            .map(|hand| hand.work_id.clone());
        let Some(work_id) = work_id else {
            continue;
        };
        let Ok(Some(run)) = store.active_run(&work_id) else {
            continue;
        };
        if run.state.is_terminal() {
            continue;
        }
        super::finish_run(
            world,
            run,
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        );
    }
}

/// Answers a permission request whose approval the user, the head or the card
/// resolved, by resuming the parked turn with the selected outcome.
fn resume_approvals(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let resolved: Vec<(String, String, RequestPermissionOutcome)> = {
        let hands = world.resource::<Hands>();
        hands
            .by_adapter
            .iter()
            .filter_map(|(adapter, hand)| {
                let pending = hand.pending.as_ref()?;
                store.active_run(&hand.work_id).ok().flatten()?;
                let Ok(Some(approval)) = store.approval(&pending.approval) else {
                    return None;
                };
                let outcome = match approval.status {
                    ApprovalStatus::Approved => outcome(&pending.options, true),
                    ApprovalStatus::Denied => outcome(&pending.options, false),
                    // The hold was released without a decision (a stop took
                    // its call): answer so the guest's turn state does not
                    // dangle.
                    ApprovalStatus::Expired => RequestPermissionOutcome::Cancelled,
                    ApprovalStatus::Pending => return None,
                };
                Some((adapter.clone(), pending.request.clone(), outcome))
            })
            .collect()
    };
    for (adapter, request, reply) in resolved {
        if let Some(hand) = world.resource_mut::<Hands>().by_adapter.get_mut(&adapter) {
            hand.pending = None;
        }
        dispatch(
            world,
            &adapter,
            HarnessInput::Respond {
                request_id: request,
                outcome: reply,
            },
        );
    }
}

/// Starts the prompt turn the parked input waited for, once the session is
/// idle and open again. The rig hand answers mid-run input inside its turn;
/// a harness guest cannot, so the queue lands as the next turn instead.
fn deliver_parked(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let due: Vec<(String, String)> = {
        let mut hands = world.resource_mut::<Hands>();
        hands
            .by_adapter
            .iter_mut()
            .filter_map(|(_, hand)| {
                if hand.parked.is_empty() {
                    return None;
                }
                if store.active_run(&hand.work_id).ok().flatten().is_some() {
                    return None;
                }
                let Ok(Some(session)) = store.work_session(&hand.work_id) else {
                    return None;
                };
                if session.status == WorkStatus::Closed {
                    return None;
                }
                let text = hand.parked.drain(..).collect::<Vec<_>>().join("\n\n");
                Some((hand.work_id.clone(), text))
            })
            .collect()
    };
    for (work_id, text) in due {
        let Ok(Some(session)) = store.work_session(&work_id) else {
            continue;
        };
        if let Err(error) = super::start_run(world, &session, &text) {
            tracing::warn!(work = %work_id, "parked input could not start a run: {error}");
        }
    }
}
