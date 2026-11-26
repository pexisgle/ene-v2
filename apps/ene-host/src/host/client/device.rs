//! Device operations over `ClientService.DeviceStream`.
//!
//! A capability tool call is dispatched to the client bound when the gate
//! accepted it. The host writes the operation first, then tries to deliver it;
//! a client that is offline leaves it `pending` until it reconnects. A
//! delivered operation whose result never arrives is `uncertain`, never a
//! success.
//!
//! Writing and delivery live in [`device_dispatch`]; the client's answer in
//! [`device_result`].
use super::{
    DeviceRequestArgs,
    computer::{self, ComputerUse},
};
use crate::host::{
    client::{CapabilitySpec, DeviceStatus},
    db::{Result, Store},
    error::HostError,
    permission::{self, CallBinding},
    resources::{CompanionAgent, DeviceHub, DeviceTimeout, Waiting, WaitingOperation, WorkAgent},
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::json;
use std::{fmt::Write as _, sync::Arc, time::Instant};

pub(crate) use super::device_dispatch::{accept, disconnected, dispatch_direct, resend};
pub(crate) use super::device_result::complete;

/// A generic `device_request` call. The tool waits for the client's result.
pub(crate) fn on_device_request(called: On<ToolCalled<DeviceRequestArgs>>, mut commands: Commands) {
    let called = called.event();
    let (call, agent, args) = (called.call, called.agent, called.args.clone());
    commands.queue(move |world: &mut World| {
        let store = world.resource::<Store>().clone();
        let binding = world.get::<CallBinding>(call).cloned();
        let Some(spec) = binding_spec(&store, binding.as_ref(), &args.capability) else {
            permission::refuse(
                world,
                call,
                &format!("capability `{}` is not declared", args.capability),
            );
            return;
        };
        match accept(world, call, agent, spec.as_ref(), args.input.clone()) {
            Ok((operation_id, _)) => {
                let target_deadline = deadline(world.resource::<DeviceTimeout>());
                wait(
                    &mut world.resource_mut::<Waiting>(),
                    call,
                    &operation_id,
                    target_deadline,
                );
            }
            Err(error) => refuse_error(world, call, error),
        }
    });
}

type NewTurnCall = (Added<Admitted>, With<TurnCall>);

/// Give each model request the capabilities of the same client the gate uses.
/// Keep each input schema standalone so its local references retain their scope.
pub(super) fn describe_device_request(
    mut calls: Query<(&mut ModelRequest, &ForAgent), NewTurnCall>,
    agents: Query<(
        Option<&CompanionAgent>,
        Option<&WorkAgent>,
        Option<&Connection>,
    )>,
    store: Res<Store>,
    hub: Res<DeviceHub>,
    computer_use: Res<ComputerUse>,
    mut commands: Commands,
) {
    for (mut call, agent) in &mut calls {
        let Some(index) = call
            .request
            .tools
            .iter()
            .position(|tool| tool.name.as_str() == "device_request")
        else {
            continue;
        };
        let (target, work, connection) = match agents.get(agent.0) {
            Ok((Some(companion), _, model)) => (
                store.client_repo().target(&companion.companion_id, None),
                None,
                model,
            ),
            Ok((_, Some(work), model)) => (
                store
                    .client_repo()
                    .target(&work.companion_id, Some(&work.work_id)),
                Some(work),
                model,
            ),
            _ => continue,
        };
        let computer_model = computer::model_supported(call.connection.as_ref().or(connection));
        if computer_model && let Some(work) = work {
            computer_use.attach_frame(&mut call, work, &store, &hub);
        }
        let target_id = match target {
            Ok(id) => id,
            Err(error) => {
                if error.is_storage() {
                    let message = error.to_string();
                    commands.queue(move |world: &mut World| {
                        crate::host::fail(world, message);
                    });
                    return;
                }
                None
            }
        };
        let client = match target_id {
            Some(id) => match store.client_repo().client(&id) {
                Ok(client) => client,
                Err(error) => {
                    if error.is_storage() {
                        let message = error.to_string();
                        commands.queue(move |world: &mut World| {
                            crate::host::fail(world, message);
                        });
                        return;
                    }
                    None
                }
            },
            None => None,
        };
        let mut names = Vec::new();
        let mut inputs = String::from("JSON input for the selected capability.");
        if let Some(client) = client {
            for name in &client.granted_capabilities {
                if permission::hard_denied(name).is_some()
                    || (work.is_none() && name.starts_with(super::WORKSPACE_CAPABILITY_PREFIX))
                    || (name == w::COMPUTER_USE
                        && (work.is_none() || !hub.connected(&client.id) || !computer_model))
                {
                    continue;
                }
                names.push(name.clone());
                if let Some(spec) = client.spec(name)
                    && let Ok(schema) = ene_host_core::json::parse_schema(&spec.input_schema)
                {
                    let _discarded = write!(
                        inputs,
                        "\n{name}: {} Input schema: {schema}",
                        spec.tool_description()
                    );
                }
            }
        }
        let tools = &mut call.request_mut().tools;
        if names.is_empty() {
            tools.remove(index);
        } else {
            let parameters = &mut Arc::make_mut(&mut tools[index]).parameters;
            parameters["properties"]["capability"] = json!({"type": "string", "enum": names});
            parameters["properties"]["input"] = json!({"description": inputs});
        }
    }
}

/// The declaration the bound client currently carries for `capability`.
fn binding_spec(
    store: &Store,
    binding: Option<&CallBinding>,
    capability: &str,
) -> Option<Option<CapabilitySpec>> {
    let binding = binding?;
    let client = store
        .client_repo()
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

fn get_state(
    store: &Store,
    computer_use: Option<&ComputerUse>,
    hub: Option<&DeviceHub>,
    id: &str,
) -> Result<w::DeviceOperationState> {
    let mut operation = store
        .client_repo()
        .operation(id)?
        .ok_or_else(|| HostError::NotFound(("device operation not found").into()))?;
    if operation.capability == w::COMPUTER_USE
        && !operation.status.is_terminal()
        && !computer_use.is_some_and(|state| {
            state
                .operations
                .get(&operation.id)
                .is_some_and(|pending| hub.is_some_and(|hub| computer::active(store, hub, pending)))
        })
    {
        operation.status = DeviceStatus::Uncertain;
    }
    Ok(wire::device_state(&operation))
}

/// Reads the current state of one operation.
pub(crate) fn get(world: &mut World, request: &w::Id) -> Result<w::DeviceOperationState> {
    get_state(
        world.resource::<Store>(),
        world.get_resource::<ComputerUse>(),
        world.get_resource::<DeviceHub>(),
        &request.id,
    )
}

/// The waiting deadline expired: the operation is uncertain, never a success.
pub(crate) fn expire(world: &mut World, operation_id: &str) {
    uncertain(
        world,
        operation_id,
        "device operation timed out; its result is unknown",
    );
}

pub(super) fn uncertain(world: &mut World, operation_id: &str, reason: &str) {
    if let Some(mut computer) = world.get_resource_mut::<ComputerUse>() {
        computer.operations.remove(operation_id);
    }
    let store = world.resource::<Store>().clone();
    let Ok(Some(operation)) = store.client_repo().operation(operation_id) else {
        return;
    };
    if !operation.status.is_terminal() {
        let _written = store
            .client_repo()
            .set_operation_status(operation_id, DeviceStatus::Uncertain);
    }
    let waiter = world
        .resource_mut::<Waiting>()
        .by_operation
        .remove(operation_id);
    if let Some(waiter) = waiter {
        permission::refuse(world, waiter.call, reason);
    }
}

/// Registers a waiting tool call for an operation.
fn wait(waiting: &mut Waiting, call: Entity, operation: &str, deadline: Instant) {
    waiting
        .by_operation
        .insert(operation.to_owned(), WaitingOperation { call, deadline });
}

/// The default deadline for a device operation.
fn deadline(timeout: &DeviceTimeout) -> Instant {
    Instant::now() + timeout.0
}
