//! Writing, delivering and replaying device operations.
//!
//! The operation is written first and delivered second. A client with no live
//! stream leaves it `pending` for the reconnect, and a replay never repeats a
//! Computer Use operation.

use super::{
    CapabilitySpec, DeviceOperation, DeviceStatus,
    computer::{self, ComputerUse},
    device::uncertain,
};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    notify::Notifier,
    permission::{self, CallBinding},
    resources::{CompanionAgent, DeviceHub, WorkAgent},
    util::now,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::model::Connection;
use serde_json::{Value, json};

/// Writes and dispatches one operation, returning its id and the immediate
/// tool answer. The answer reports the status dispatch reached: `dispatched`
/// when the client stream accepted the operation, `pending` when no live
/// stream has it yet. It never reports completion.
/// The operation goes to the client the gate bound the call to; a client that
/// moved since then does not void it.
pub(crate) fn accept(
    world: &mut World,
    call: Entity,
    agent: Entity,
    spec: Option<&CapabilitySpec>,
    input: Value,
) -> Result<(String, String)> {
    let store = world.resource::<Store>().clone();
    let binding = world
        .get::<CallBinding>(call)
        .cloned()
        .ok_or_else(|| HostError::Internal(("a device call without a binding").into()))?;
    let (companion_id, work_id) = agent_context(world, agent)
        .ok_or_else(|| HostError::Internal(("a device call without an agent").into()))?;
    let client = store
        .client_repo()
        .client(&binding.client_id)?
        .ok_or_else(|| HostError::Conflict(("the bound client is no longer registered").into()))?;
    if !client
        .capabilities
        .iter()
        .any(|name| name == &binding.capability)
    {
        return Err(HostError::Conflict(
            ("the capability is no longer declared").into(),
        ));
    }
    if !client.is_granted(&binding.capability) {
        return Err(HostError::Conflict(
            ("the capability is no longer granted").into(),
        ));
    }
    if let Some(spec) = spec {
        if spec.name != binding.capability {
            return Err(HostError::Conflict(
                ("the capability no longer matches the accepted operation").into(),
            ));
        }
        super::validate_input(spec, &input)?;
    }
    let action = permission::CandidateAction::DeviceCapability {
        capability: binding.capability.clone(),
        spec: spec.cloned().map(Box::new),
        tool_name: binding.capability.clone(),
        input: input.clone(),
        raw_arguments: input.clone(),
    };
    let typed_target = permission::extract_target(&action);
    if let Some(reason) = permission::hard_denied_action(&action, &typed_target) {
        return Err(HostError::Permission(reason));
    }
    let computer = if binding.capability == w::COMPUTER_USE {
        let work = world.get::<WorkAgent>(agent).ok_or_else(|| {
            HostError::Permission(("Computer Use requires a work session").into())
        })?;
        let run = binding
            .computer_run
            .as_deref()
            .ok_or_else(|| HostError::Internal(("Computer Use has no bound run").into()))?;
        if spec.is_none() || !computer::model_supported(world.get::<Connection>(agent)) {
            return Err(HostError::Permission(
                ("Computer Use requires a declared schema and image capable task model").into(),
            ));
        }
        Some(computer::prepare(
            computer::ComputerState {
                store: world.resource::<Store>(),
                hub: world.resource::<DeviceHub>(),
                computer: world.resource::<computer::ComputerUse>(),
            },
            work,
            &binding.client_id,
            run,
            &input,
        )?)
    } else {
        None
    };
    // The work session is recorded with the operation; the dispatch target is
    // the bound client.
    let operation_id = uuid::Uuid::new_v4().to_string();
    let operation = DeviceOperation {
        id: operation_id.clone(),
        client_id: binding.client_id.clone(),
        capability: binding.capability.clone(),
        input,
        work_id: work_id.clone(),
        status: DeviceStatus::Pending,
        output: None,
        failure: None,
        created_at: now(),
        updated_at: now(),
    };
    store.client_repo().insert_operation(&operation)?;
    let status = {
        let hub = world.resource::<DeviceHub>().clone();
        let mut notifier = world.resource_mut::<Notifier>();
        let mut ctx = DeviceDispatchContext {
            store: &store,
            hub: &hub,
            notifier: &mut notifier,
        };
        dispatch(&mut ctx, &operation, computer.as_ref(), Some(&companion_id))?
    };
    if let Some(computer) = computer {
        if status != DeviceStatus::Dispatched {
            store
                .client_repo()
                .set_operation_status(&operation.id, DeviceStatus::Failed)?;
            return Err(HostError::Conflict(
                ("the Computer Use device stream is unavailable").into(),
            ));
        }
        let mut state = world.resource_mut::<ComputerUse>();
        state.frames.remove(&operation.client_id);
        state.operations.insert(operation_id.clone(), computer);
    }
    let answer = json!({"operation_id": &operation_id, "status": status.as_str()}).to_string();
    Ok((operation_id, answer))
}

/// Dispatches an authorized host-initiated capability operation (such as an
/// `Extension::Character` expression/motion action) directly to `client_id`
/// when the client has declared and been granted `capability`.
pub(crate) fn dispatch_direct(
    world: &mut World,
    companion_id: &str,
    client_id: &str,
    capability: &str,
    input: Value,
) -> Result<String> {
    let store = world.resource::<Store>().clone();
    let client = store
        .client_repo()
        .client(client_id)?
        .ok_or_else(|| HostError::NotFound("client not found".into()))?;
    if !client.is_granted(capability) {
        return Err(HostError::Permission(format!(
            "capability `{capability}` is not granted on `{client_id}`"
        )));
    }
    let operation_id = uuid::Uuid::new_v4().to_string();
    let operation = DeviceOperation {
        id: operation_id.clone(),
        client_id: client_id.to_owned(),
        capability: capability.to_owned(),
        input,
        work_id: None,
        status: DeviceStatus::Pending,
        output: None,
        failure: None,
        created_at: now(),
        updated_at: now(),
    };
    store.client_repo().insert_operation(&operation)?;
    let hub = world.resource::<DeviceHub>().clone();
    let mut notifier = world.resource_mut::<Notifier>();
    let mut ctx = DeviceDispatchContext {
        store: &store,
        hub: &hub,
        notifier: &mut notifier,
    };
    let _reply = dispatch(&mut ctx, &operation, None, Some(companion_id))?;
    Ok(operation_id)
}

/// `(companion_id, work_id)` for the agent entity that requested the call.
fn agent_context(world: &World, agent: Entity) -> Option<(String, Option<String>)> {
    if let Some(companion) = world.get::<CompanionAgent>(agent) {
        return Some((companion.companion_id.clone(), None));
    }
    world
        .get::<WorkAgent>(agent)
        .map(|work| (work.companion_id.clone(), Some(work.work_id.clone())))
}

struct DeviceDispatchContext<'a> {
    store: &'a Store,
    hub: &'a DeviceHub,
    notifier: &'a mut Notifier,
}

/// Writes the operation to the client's live stream. A missing stream leaves
/// the operation pending for the reconnect and is reported as such; a sent
/// operation is `dispatched`.
fn dispatch(
    ctx: &mut DeviceDispatchContext<'_>,
    operation: &DeviceOperation,
    computer: Option<&computer::Pending>,
    companion_id: Option<&str>,
) -> Result<DeviceStatus> {
    let Some(subscription) = ctx.hub.subscription(&operation.client_id) else {
        tracing::debug!(operation = %operation.id, "no device stream; the operation waits");
        return Ok(DeviceStatus::Pending);
    };
    if computer.is_some_and(|pending| pending.subscription != subscription) {
        return Ok(DeviceStatus::Pending);
    }
    let resolved_companion = match companion_id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => resolve_operation_companion(ctx.store, operation)?,
    };
    let mut wire_operation = wire::device_operation(operation, &subscription);
    wire_operation.run_id = computer.map(|pending| pending.run.clone());
    wire_operation.companion_id = resolved_companion;
    let event = w::DeviceEvent {
        event: Some(w::device_event::Event::Operation(Box::new(
            wire_operation.clone(),
        ))),
        ..Default::default()
    };
    if ctx.hub.send(&operation.client_id, event) {
        ctx.store
            .client_repo()
            .set_operation_status(&operation.id, DeviceStatus::Dispatched)?;
        ctx.notifier.push(
            None,
            Some(&operation.client_id),
            w::notification::Event::DeviceOperation(Box::new(wire_operation)),
        );
        return Ok(DeviceStatus::Dispatched);
    }

    Ok(DeviceStatus::Pending)
}

fn resolve_operation_companion(store: &Store, operation: &DeviceOperation) -> Result<String> {
    if let Some(work_id) = operation.work_id.as_deref()
        && let Some(session) = store.work_repo().work_session(work_id)?
    {
        return Ok(session.companion_id.as_str().to_owned());
    }
    Ok(store
        .client_repo()
        .activation_for_client(&operation.client_id)?
        .unwrap_or_default())
}

struct ResendPlan {
    active_companion: Option<String>,
    operations: Vec<DeviceOperation>,
}

fn load_resend_plan(
    store: &Store,
    client: &str,
) -> std::result::Result<ResendPlan, Option<String>> {
    let active_companion = match store.client_repo().activation_for_client(client) {
        Ok(companion) => companion,
        Err(error) => {
            if error.is_storage() {
                return Err(Some(error.to_string()));
            }
            tracing::warn!(client, %error, "failed to check client activation on resend");
            return Err(None);
        }
    };
    let operations = match store.client_repo().outstanding_operations(client) {
        Ok(operations) => operations,
        Err(error) => {
            if error.is_storage() {
                return Err(Some(error.to_string()));
            }
            tracing::warn!(client, %error, "failed to read outstanding operations on resend");
            return Err(None);
        }
    };
    let operations = operations
        .into_iter()
        .filter(|op| {
            op.status == DeviceStatus::Pending
                && (op.work_id.is_some() || active_companion.is_some())
        })
        .collect();
    Ok(ResendPlan {
        active_companion,
        operations,
    })
}

/// Replays operations that were never delivered after a client reconnects.
pub(crate) fn resend(world: &mut World, client: &str) {
    let subscription = world.resource::<DeviceHub>().subscription(client);
    computer::invalidate_client(world, client, subscription.as_deref());
    let store = world.resource::<Store>().clone();
    let plan = match load_resend_plan(&store, client) {
        Ok(plan) => plan,
        Err(Some(fatal)) => {
            crate::host::fail(world, fatal);
            return;
        }
        Err(None) => return,
    };
    let hub = world.resource::<DeviceHub>().clone();
    for operation in plan.operations {
        if operation.capability == w::COMPUTER_USE {
            uncertain(
                world,
                &operation.id,
                "Computer Use operations are never replayed",
            );
        } else {
            let mut notifier = world.resource_mut::<Notifier>();
            let mut ctx = DeviceDispatchContext {
                store: &store,
                hub: &hub,
                notifier: &mut notifier,
            };
            let _reply = dispatch(&mut ctx, &operation, None, plan.active_companion.as_deref());
        }
    }
}

/// Called when a client's stream ends. Dispatched operations whose answer can
/// no longer arrive become uncertain; they are not treated as successes.
pub(crate) fn disconnected(world: &mut World, client: &str) {
    computer::invalidate_client(world, client, None);
    let store = world.resource::<Store>().clone();
    let operations = match store.client_repo().outstanding_operations(client) {
        Ok(operations) => operations,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            } else {
                tracing::warn!(client, %error, "failed to read operations on disconnect");
            }
            return;
        }
    };
    for operation in operations {
        if operation.status != DeviceStatus::Dispatched {
            continue;
        }
        if let Err(error) = store
            .client_repo()
            .set_operation_status(&operation.id, DeviceStatus::Uncertain)
            && error.is_storage()
        {
            crate::host::fail(world, error.to_string());
            return;
        }
    }
}
