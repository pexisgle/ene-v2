//! Device operations over `ClientService.DeviceStream`.
//!
//! A capability tool call is dispatched to the client bound when the gate
//! accepted it. The host writes the operation first, then tries to deliver it;
//! a client that is offline leaves it `pending` until it reconnects. A
//! delivered operation whose result never arrives is `uncertain`, never a
//! success.
use super::DeviceRequestArgs;
use crate::host::{
    agent::{CompanionAgent, WorkAgent},
    client::{CapabilitySpec, DeviceOperation, DeviceStatus},
    db::{Result, Store, now},
    error::{Failure, HostError},
    notify::Notifier,
    permission::{self, CallBinding},
    resources::{DeviceHub, DeviceTimeout, Waiting, WaitingOperation},
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::time::Instant;

/// A generic `device_request` call. The tool waits for the client's result.
pub(crate) fn on_device_request(called: On<ToolCalled<DeviceRequestArgs>>, mut commands: Commands) {
    let called = called.event();
    let (call, agent, args) = (called.call, called.agent, called.args.clone());
    commands.queue(move |world: &mut World| {
        let Some(spec) = binding_spec(world, call, &args.capability) else {
            permission::refuse(
                world,
                call,
                &format!("capability `{}` is not declared", args.capability),
            );
            return;
        };
        if let Err(error) = execute(world, call, agent, spec.as_ref(), args.input.clone(), true) {
            refuse_error(world, call, error);
        }
    });
}

/// The declaration the bound client currently carries for `capability`.
fn binding_spec(world: &World, call: Entity, capability: &str) -> Option<Option<CapabilitySpec>> {
    let binding = world.get::<CallBinding>(call)?;
    let client = world
        .resource::<Store>()
        .client(&binding.client_id)
        .ok()
        .flatten()?;
    client
        .capabilities
        .iter()
        .any(|name| name == capability)
        .then(|| client.spec(capability).cloned())
}

/// A refused device call keeps its specific conflict/permission message and
/// stringifies anything else.
fn refuse_error(world: &mut World, call: Entity, error: HostError) {
    match error {
        HostError::Conflict(reason) | HostError::Permission(reason) => {
            permission::refuse(world, call, &reason)
        }
        other => permission::refuse(world, call, &other.to_string()),
    }
}

/// Accepts one capability operation against the client the call is bound to.
///
/// `wait_for_result` is the documented split (`docs/api.md` §クライアント能力):
/// `device_request` passes `true` and the call stays open — a [`Waiting`]
/// entry, bounded by [`DeviceTimeout`], is answered by [`complete`] or
/// [`expire`]. A named capability tool passes `false` and answers immediately
/// with the operation id and the dispatch status; its failure or non-delivery
/// is observed through `GetOperation` or the device notification.
pub(crate) fn execute(
    world: &mut World,
    call: Entity,
    agent: Entity,
    spec: Option<&CapabilitySpec>,
    input: Value,
    wait_for_result: bool,
) -> Result<()> {
    let (operation_id, answer) = accept(world, call, agent, spec, input)?;
    if wait_for_result {
        wait(world, call, &operation_id, deadline(world));
    } else {
        permission::answer(world, call, &answer);
    }
    Ok(())
}

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
    let work_id = agent_context(world, agent)
        .ok_or_else(|| HostError::Internal(("a device call without an agent").into()))?;
    let client = store
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
    store.insert_operation(&operation)?;
    let status = dispatch(world, &operation)?;
    let answer = json!({"operation_id": &operation_id, "status": status.as_str()}).to_string();
    Ok((operation_id, answer))
}

/// The work session an agent serves: `Some(None)` for a companion agent,
/// `Some(Some(work))` for a hand, and `None` for an entity that serves
/// neither.
fn agent_context(world: &World, agent: Entity) -> Option<Option<String>> {
    if world.get::<CompanionAgent>(agent).is_some() {
        return Some(None);
    }
    world
        .get::<WorkAgent>(agent)
        .map(|work| Some(work.work_id.clone()))
}

/// Writes the operation to the client's live stream. A missing stream leaves
/// the operation pending for the reconnect and is reported as such; a sent
/// operation is `dispatched`.
fn dispatch(world: &mut World, operation: &DeviceOperation) -> Result<DeviceStatus> {
    let hub = world.resource::<DeviceHub>().clone();
    let store = world.resource::<Store>().clone();
    let Some(subscription) = hub.subscription(&operation.client_id) else {
        tracing::debug!(operation = %operation.id, "no device stream; the operation waits");
        return Ok(DeviceStatus::Pending);
    };
    let event = w::DeviceEvent {
        event: Some(w::device_event::Event::Operation(Box::new(
            wire::device_operation(operation, &subscription),
        ))),
        ..Default::default()
    };
    if hub.send(&operation.client_id, event) {
        store.set_operation_status(&operation.id, DeviceStatus::Dispatched)?;
        let mut notifier = world.resource_mut::<Notifier>();
        notifier.push(
            None,
            Some(&operation.client_id),
            w::notification::Event::DeviceOperation(Box::new(wire::device_operation(
                operation,
                &subscription,
            ))),
        );
        return Ok(DeviceStatus::Dispatched);
    }

    Ok(DeviceStatus::Pending)
}

/// Replays operations that were never delivered after a client reconnects.
pub(crate) fn resend(world: &mut World, client: &str) {
    let store = world.resource::<Store>().clone();
    if !matches!(store.activation_for_client(client), Ok(Some(_))) {
        return;
    }
    let Ok(operations) = store.outstanding_operations(client) else {
        return;
    };
    for operation in operations {
        if operation.status != DeviceStatus::Pending {
            continue;
        }
        let _ = dispatch(world, &operation);
    }
}

/// Called when a client's stream ends. Dispatched operations whose answer can
/// no longer arrive become uncertain; they are not treated as successes.
pub(crate) fn disconnected(world: &mut World, client: &str) {
    let store = world.resource::<Store>().clone();
    let operations = store.outstanding_operations(client).unwrap_or_default();
    for operation in operations {
        if operation.status != DeviceStatus::Dispatched {
            continue;
        }
        let _ = store.set_operation_status(&operation.id, DeviceStatus::Uncertain);
    }
}

/// Records a client's result and answers the waiting tool call. The result is
/// matched to its operation by id; the client id it carries is the record.
pub(crate) fn complete(
    world: &mut World,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let Some(operation) = store.operation(&result.operation_id)? else {
        return Err(HostError::NotFound(("device operation not found").into()));
    };
    if operation.status.is_terminal() {
        return Ok(wire::device_state(&operation));
    }
    // A capability without an output schema may answer with anything, even
    // an empty body; with a schema, validation happens below.
    let output = if result.output.trim().is_empty() {
        Some(Value::Null)
    } else {
        Some(
            serde_json::from_str::<Value>(&result.output)
                .unwrap_or_else(|_| Value::String(result.output.clone())),
        )
    };
    let failure = result
        .failure
        .as_option()
        .cloned()
        .map(|failure| wire::failure_from(&failure));
    let success = result.success && failure.is_none() && output.is_some();
    // A capability declared without a spec has no output contract; its
    // answer is accepted as the client sent it.
    let validated = output.as_ref().map(|output| {
        match store
            .client(&operation.client_id)
            .ok()
            .flatten()
            .and_then(|client| client.spec(&operation.capability).cloned())
        {
            Some(spec) => super::validate_output(&spec, output)
                .map(|()| output.clone())
                .map_err(|error| error.to_string()),
            None => Ok(output.clone()),
        }
    });
    let (success, output, failure) = match (success, validated) {
        (true, Some(Ok(output))) => (true, Some(output), failure),
        (true, Some(Err(error))) => (false, None, Some(Failure::new("output_invalid", error))),
        (true, None) => (
            false,
            None,
            Some(Failure::new(
                "output_invalid",
                "the client declared no output schema",
            )),
        ),
        (false, _) => (
            false,
            None,
            failure.or_else(|| Some(Failure::new("operation_failed", "the operation failed"))),
        ),
    };
    let updated = store
        .complete_operation(&operation.id, success, output.as_ref(), failure.as_ref())?
        .ok_or_else(|| HostError::Storage(("device operation update did not persist").into()))?;
    let waiter = world
        .resource_mut::<Waiting>()
        .by_operation
        .remove(&operation.id);
    if let Some(waiter) = waiter {
        if success {
            permission::answer(
                world,
                waiter.call,
                &json!({"operation_id": operation.id, "output": output}).to_string(),
            );
        } else {
            let reason = failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "the operation failed".into());
            permission::refuse(world, waiter.call, &reason);
        }
    }
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        None,
        Some(&operation.client_id),
        w::notification::Event::DeviceOperationResult(Box::new(wire::device_result(&updated))),
    );
    Ok(wire::device_state(&updated))
}

/// Reads the current state of one operation.
pub(crate) fn get(world: &mut World, request: w::Id) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let operation = store
        .operation(&request.id)?
        .ok_or_else(|| HostError::NotFound(("device operation not found").into()))?;
    Ok(wire::device_state(&operation))
}

/// The waiting deadline expired: the operation is uncertain, never a success.
pub(crate) fn expire(world: &mut World, operation_id: &str) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(operation)) = store.operation(operation_id) else {
        return;
    };
    if !operation.status.is_terminal() {
        let _ = store.set_operation_status(operation_id, DeviceStatus::Uncertain);
    }
    let waiter = world
        .resource_mut::<Waiting>()
        .by_operation
        .remove(operation_id);
    if let Some(waiter) = waiter {
        permission::refuse(
            world,
            waiter.call,
            "device operation timed out; its result is unknown",
        );
    }
}

/// Registers a waiting tool call for an operation.
pub(crate) fn wait(world: &mut World, call: Entity, operation: &str, deadline: Instant) {
    world
        .resource_mut::<Waiting>()
        .by_operation
        .insert(operation.to_owned(), WaitingOperation { call, deadline });
}

/// The default deadline for a device operation.
pub(crate) fn deadline(world: &World) -> Instant {
    Instant::now() + world.resource::<DeviceTimeout>().0
}
