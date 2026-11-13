//! Device operations over `ClientService.DeviceStream`.
//!
//! A capability tool call is accepted against a client generation. The host
//! writes the operation first, then tries to deliver it; a client that is
//! offline leaves it `pending` until it reconnects. A delivered operation
//! whose result never arrives is `uncertain`, never a success.
use super::{CapabilityTool, DeviceRequestArgs, Slots};
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
use rig_ecs::prelude::ToolCalled;
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::time::Instant;

/// Runs once after plugin build: disables every slot until a capability is
/// published and remembers the slot entities.
pub(crate) fn init_slots(world: &mut World) {
    let mut entities = Vec::new();
    for index in 0..crate::host::client::SLOTS {
        let name = format!("ene_cap_{index}");
        let found = {
            let mut query = world.query::<(Entity, &rig_ecs::prelude::ToolDef)>();
            query
                .iter(world)
                .find(|(_, definition)| definition.0.name.as_str() == name)
                .map(|(entity, _)| entity)
        };
        if let Some(entity) = found {
            world.entity_mut(entity).insert(Disabled);
            entities.push(entity);
        }
    }
    world.resource_mut::<Slots>().entities = entities;
    if let Err(error) = crate::host::client::sync_tools(world) {
        crate::host::fail(world, error.to_string());
    }
}

/// A call of a published capability tool. The client does not have to answer
/// immediate tools; the host returns the operation id.
pub(crate) fn on_slot_call(
    called: On<ToolCalled<Value>>,
    slots: Query<&CapabilityTool>,
    mut commands: Commands,
) {
    let called = called.event();
    let (call, agent, tool, args) = (
        called.call,
        called.agent,
        called.entity,
        called.args.clone(),
    );
    let spec = slots
        .get(tool)
        .map(|tool| tool.spec.clone())
        .unwrap_or(CapabilitySpec {
            name: String::new(),
            tool_name: String::new(),
            description: String::new(),
            input_schema: String::new(),
            presets: Vec::new(),
            approval: crate::host::client::CapabilityApproval::Unspecified,
            approval_target: String::new(),
            output_schema: String::new(),
        });
    commands.queue(move |world: &mut World| {
        if let Err(error) = execute(world, call, agent, &spec, args, false) {
            match error {
                HostError::Conflict(reason) | HostError::Permission(reason) => {
                    permission::refuse(world, call, &reason)
                }
                other => permission::refuse(world, call, &other.to_string()),
            }
        }
    });
}

/// A generic `device_request` call. The tool waits for the client's result.
pub(crate) fn on_device_request(called: On<ToolCalled<DeviceRequestArgs>>, mut commands: Commands) {
    let called = called.event();
    let (call, agent, args) = (called.call, called.agent, called.args.clone());
    commands.queue(move |world: &mut World| {
        let args = &args;
        let Some(spec) = binding_spec(world, call, &args.capability) else {
            permission::refuse(
                world,
                call,
                &format!("capability `{}` is not declared", args.capability),
            );
            return;
        };
        if let Err(error) = execute(world, call, agent, &spec, args.input.clone(), true) {
            match error {
                HostError::Conflict(reason) | HostError::Permission(reason) => {
                    permission::refuse(world, call, &reason)
                }
                other => permission::refuse(world, call, &other.to_string()),
            }
        }
    });
}

fn binding_spec(world: &World, call: Entity, capability: &str) -> Option<CapabilitySpec> {
    let binding = world.get::<CallBinding>(call)?;
    let store = world.resource::<Store>();
    store
        .client(&binding.client_id)
        .ok()
        .flatten()
        .and_then(|client| client.spec(capability).cloned())
}

/// Accepts one capability operation against the bound client generation.
pub(crate) fn execute(
    world: &mut World,
    call: Entity,
    agent: Entity,
    spec: &CapabilitySpec,
    input: Value,
    wait_for_result: bool,
) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let binding = world
        .get::<CallBinding>(call)
        .cloned()
        .ok_or_else(|| HostError::internal("a device call without a binding"))?;
    // Revalidate the generation immediately before dispatch: a moved client
    // never receives an operation accepted for the old one.
    let (companion_id, work_id) = agent_context(world, agent)
        .ok_or_else(|| HostError::internal("a device call without an agent"))?;
    let current = current_target(&store, &companion_id, work_id.as_deref())?;
    if current.as_ref() != Some(&(binding.client_id.clone(), binding.activation_revision)) {
        return Err(HostError::conflict(
            "client_moved: the active client changed before the operation was dispatched",
        ));
    }
    if spec.name != binding.capability {
        return Err(HostError::conflict(
            "the capability no longer matches the accepted operation",
        ));
    }
    super::validate_input(spec, &input)?;
    let operation_id = uuid::Uuid::new_v4().to_string();
    let operation = DeviceOperation {
        id: operation_id.clone(),
        client_id: binding.client_id.clone(),
        activation_revision: binding.activation_revision,
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
    dispatch(world, &operation, wait_for_result)?;
    if wait_for_result {
        wait(world, call, &operation_id, deadline(world));
    } else {
        permission::answer(
            world,
            call,
            &json!({"operation_id": operation_id, "status": "pending"}).to_string(),
        );
    }
    Ok(())
}

fn agent_context(world: &World, agent: Entity) -> Option<(String, Option<String>)> {
    if let Some(companion) = world.get::<CompanionAgent>(agent) {
        return Some((companion.companion_id.clone(), None));
    }
    world
        .get::<WorkAgent>(agent)
        .map(|work| (work.companion_id.clone(), Some(work.work_id.clone())))
}

fn current_target(
    store: &Store,
    companion: &str,
    work: Option<&str>,
) -> Result<Option<(String, u64)>> {
    if let Some(work) = work {
        let session = store
            .work_session(work)?
            .ok_or_else(|| HostError::not_found("work session not found"))?;
        return Ok(session
            .client_id
            .map(|client| (client, session.activation_revision)));
    }
    Ok(store.activation(companion)?.and_then(|activation| {
        activation
            .client_id
            .map(|client| (client, activation.revision))
    }))
}

/// Writes the operation to the client's live stream. A missing stream leaves
/// the operation pending for the reconnect.
fn dispatch(world: &mut World, operation: &DeviceOperation, _wait: bool) -> Result<()> {
    let hub = world.resource::<DeviceHub>().clone();
    let store = world.resource::<Store>().clone();
    let Some(subscription) = hub.subscription(&operation.client_id) else {
        tracing::debug!(operation = %operation.id, "no device stream; the operation waits");
        return Ok(());
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
    }

    Ok(())
}

/// Replays operations that were never delivered after a client reconnects.
pub(crate) fn resend(world: &mut World, client: &str) {
    let store = world.resource::<Store>().clone();
    let Ok(activation) = store.activation_for_client(client) else {
        return;
    };
    let Some((_companion, revision)) = activation else {
        return;
    };
    let Ok(operations) = store.outstanding_operations(client) else {
        return;
    };
    for operation in operations {
        if operation.status != DeviceStatus::Pending || operation.activation_revision != revision {
            continue;
        }
        let _ = dispatch(world, &operation, true);
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

/// Records a client's result and answers the waiting tool call.
pub(crate) fn complete(
    world: &mut World,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let Some(operation) = store.operation(&result.operation_id)? else {
        return Err(HostError::not_found("device operation not found"));
    };
    if result.activation_revision != operation.activation_revision
        || result.client_id != operation.client_id
    {
        // A late result from a previous generation is discarded.
        return Ok(wire::device_state(&operation));
    }
    if operation.status.is_terminal() {
        return Ok(wire::device_state(&operation));
    }
    let output = super::super::json::parse_object(&result.output).ok();
    let failure = result
        .failure
        .as_option()
        .cloned()
        .map(|failure| wire::failure_from(&failure));
    let success = result.success && failure.is_none() && output.is_some();
    let validated = output.and_then(|output| {
        store
            .client(&operation.client_id)
            .ok()
            .flatten()
            .and_then(|client| client.spec(&operation.capability).cloned())
            .map(|spec| {
                super::validate_output(&spec, &output)
                    .map(|()| output)
                    .map_err(|error| error.to_string())
            })
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
        .ok_or_else(|| HostError::storage("device operation update did not persist"))?;
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
        .ok_or_else(|| HostError::not_found("device operation not found"))?;
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

/// A move invalidates the calls that were waiting for the old generation's
/// operation results.
pub(crate) fn invalidate(world: &mut World, client: &str, revision: u64) {
    let store = world.resource::<Store>().clone();
    let waiting: Vec<String> = world
        .resource::<Waiting>()
        .by_operation
        .keys()
        .cloned()
        .collect();
    for operation in waiting {
        let moved = store
            .operation(&operation)
            .ok()
            .flatten()
            .is_some_and(|operation| {
                operation.client_id == client && operation.activation_revision == revision
            });
        if !moved {
            continue;
        }
        let waiter = world
            .resource_mut::<Waiting>()
            .by_operation
            .remove(&operation);
        if let Some(waiter) = waiter {
            permission::refuse(
                world,
                waiter.call,
                "client_moved: the active client moved before the result arrived",
            );
        }
    }
}
