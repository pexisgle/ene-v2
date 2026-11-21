//! Device operations over `ClientService.DeviceStream`.
//!
//! A capability tool call is dispatched to the client bound when the gate
//! accepted it. The host writes the operation first, then tries to deliver it;
//! a client that is offline leaves it `pending` until it reconnects. A
//! delivered operation whose result never arrives is `uncertain`, never a
//! success.
use super::{
    DeviceRequestArgs,
    computer::{self, ComputerUse},
};
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
use rig_ecs::model::Connection;
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::{fmt::Write as _, sync::Arc, time::Instant};

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
            Ok((Some(companion), _, model)) => {
                (store.target(&companion.companion_id, None), None, model)
            }
            Ok((_, Some(work), model)) => (
                store.target(&work.companion_id, Some(&work.work_id)),
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
            Some(id) => match store.client(&id) {
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
                    && let Ok(schema) = super::json::parse_schema(&spec.input_schema)
                {
                    let _ = write!(
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
    let (companion_id, work_id) = agent_context(world, agent)
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
            world,
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
    store.insert_operation(&operation)?;
    let status = dispatch(world, &operation, computer.as_ref(), Some(&companion_id))?;
    if let Some(computer) = computer {
        if status != DeviceStatus::Dispatched {
            store.set_operation_status(&operation.id, DeviceStatus::Failed)?;
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
    store.insert_operation(&operation)?;
    let _ = dispatch(world, &operation, None, Some(companion_id))?;
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

/// Writes the operation to the client's live stream. A missing stream leaves
/// the operation pending for the reconnect and is reported as such; a sent
/// operation is `dispatched`.
fn dispatch(
    world: &mut World,
    operation: &DeviceOperation,
    computer: Option<&computer::Pending>,
    companion_id: Option<&str>,
) -> Result<DeviceStatus> {
    let hub = world.resource::<DeviceHub>().clone();
    let store = world.resource::<Store>().clone();
    let Some(subscription) = hub.subscription(&operation.client_id) else {
        tracing::debug!(operation = %operation.id, "no device stream; the operation waits");
        return Ok(DeviceStatus::Pending);
    };
    if computer.is_some_and(|pending| pending.subscription != subscription) {
        return Ok(DeviceStatus::Pending);
    }
    let resolved_companion = match companion_id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => resolve_operation_companion(&store, operation)?,
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
    if hub.send(&operation.client_id, event) {
        store.set_operation_status(&operation.id, DeviceStatus::Dispatched)?;
        let mut notifier = world.resource_mut::<Notifier>();
        notifier.push(
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
        && let Some(session) = store.work_session(work_id)?
    {
        return Ok(session.companion_id);
    }
    Ok(store
        .activation_for_client(&operation.client_id)?
        .unwrap_or_default())
}

/// Replays operations that were never delivered after a client reconnects.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::cognitive_complexity)]
pub(crate) fn resend(world: &mut World, client: &str) {
    let subscription = world.resource::<DeviceHub>().subscription(client);
    computer::invalidate_client(world, client, subscription.as_deref());
    let store = world.resource::<Store>().clone();
    let active_companion = match store.activation_for_client(client) {
        Ok(companion) => companion,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            } else {
                tracing::warn!(client, %error, "failed to check client activation on resend");
            }
            return;
        }
    };
    let operations = match store.outstanding_operations(client) {
        Ok(operations) => operations,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            } else {
                tracing::warn!(client, %error, "failed to read outstanding operations on resend");
            }
            return;
        }
    };
    for operation in operations {
        if operation.status != DeviceStatus::Pending {
            continue;
        }
        if operation.work_id.is_none() && active_companion.is_none() {
            continue;
        }
        if operation.capability == w::COMPUTER_USE {
            uncertain(
                world,
                &operation.id,
                "Computer Use operations are never replayed",
            );
        } else {
            let _ = dispatch(world, &operation, None, active_companion.as_deref());
        }
    }
}

/// Called when a client's stream ends. Dispatched operations whose answer can
/// no longer arrive become uncertain; they are not treated as successes.
pub(crate) fn disconnected(world: &mut World, client: &str) {
    computer::invalidate_client(world, client, None);
    let store = world.resource::<Store>().clone();
    let operations = match store.outstanding_operations(client) {
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
        if let Err(error) = store.set_operation_status(&operation.id, DeviceStatus::Uncertain)
            && error.is_storage()
        {
            crate::host::fail(world, error.to_string());
            return;
        }
    }
}

/// Records a client's result and answers the waiting tool call. The result is
/// matched to its operation by id; the client id it carries is the record.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_lines)]
pub(crate) fn complete(
    world: &mut World,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let Some(operation) = store.operation(&result.operation_id)? else {
        return Err(HostError::NotFound(("device operation not found").into()));
    };
    if operation.capability == w::COMPUTER_USE && operation.client_id != result.client_id {
        return Err(HostError::Permission(
            ("the result belongs to another client").into(),
        ));
    }
    if operation.status.is_terminal() {
        return Ok(wire::device_state(&operation));
    }
    let computer = if operation.capability == w::COMPUTER_USE {
        let pending = world
            .resource::<ComputerUse>()
            .operations
            .get(&operation.id)
            .cloned();
        if !pending
            .as_ref()
            .is_some_and(|pending| computer::active(&store, world.resource::<DeviceHub>(), pending))
            || result
                .failure
                .as_option()
                .is_some_and(|failure| failure.code == "operation_uncertain")
        {
            uncertain(
                world,
                &operation.id,
                "the Computer Use operation's result is unknown; do not retry it",
            );
            return get(
                world,
                w::Id {
                    id: operation.id,
                    ..Default::default()
                },
            );
        }
        pending
    } else {
        None
    };
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
    let spec = store
        .client(&operation.client_id)?
        .and_then(|client| client.spec(&operation.capability).cloned());
    let mut validated = output.map(|output| match spec {
        Some(spec) => super::validate_output(&spec, &output)
            .map(|()| output)
            .map_err(|error| error.to_string()),
        None => Ok(output),
    });
    let mut frame = None;
    if let Some(pending) = computer {
        world
            .resource_mut::<ComputerUse>()
            .operations
            .remove(&operation.id);
        validated = match (success, validated) {
            (true, Some(Ok(output))) => Some(computer::result(pending, output).map(
                |(metadata, captured)| {
                    frame = captured;
                    metadata
                },
            )),
            (_, Some(Err(_))) => Some(Err(
                "the client returned an invalid Computer Use result".into()
            )),
            (_, validated) => validated,
        };
    }
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
            if let Some(frame) = frame {
                computer::save_frame(world, frame);
            }
            if let Some(image) = read_image(&operation.capability, output.as_ref()) {
                use rig_core::{
                    message::ToolResultContent,
                    tool::{ToolOutput, ToolResult},
                };
                let blocks = vec![
                    ToolResultContent::text(json!({
                        "operation_id": operation.id, "path": output.as_ref().and_then(|out| out["path"].as_str())
                    }).to_string()),
                    ToolResultContent::Image(image),
                ];
                match ToolOutput::content(blocks) {
                    Ok(tool_output) => permission::decide(
                        world,
                        waiter.call,
                        Outcome::from(ToolResult::success(tool_output)),
                    ),
                    Err(error) => {
                        permission::refuse(world, waiter.call, &error.to_string());
                    }
                }
            } else {
                permission::answer(
                    world,
                    waiter.call,
                    &json!({"operation_id": operation.id, "output": output}).to_string(),
                );
            }
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

fn read_image(capability: &str, output: Option<&Value>) -> Option<rig_core::message::Image> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use rig_core::message::{DocumentSourceKind, Image, ImageMediaType};
    if capability != "work.read" {
        return None;
    }
    let encoded = output?.get("image_base64")?.as_str()?;
    const IMAGE_LIMIT: usize = 512 * 1024;
    if encoded.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
        return None;
    }
    let bytes = STANDARD.decode(encoded).ok()?;
    if bytes.len() > IMAGE_LIMIT {
        return None;
    }
    let media_type = ImageMediaType::sniff(&bytes)?;
    Some(Image {
        data: DocumentSourceKind::Base64(encoded.to_owned()),
        media_type: Some(media_type),
        ..Default::default()
    })
}

/// Reads the current state of one operation.
pub(crate) fn get(world: &mut World, request: w::Id) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let mut operation = store
        .operation(&request.id)?
        .ok_or_else(|| HostError::NotFound(("device operation not found").into()))?;
    if operation.capability == w::COMPUTER_USE
        && !operation.status.is_terminal()
        && !world.get_resource::<ComputerUse>().is_some_and(|state| {
            state.operations.get(&operation.id).is_some_and(|pending| {
                computer::active(&store, world.resource::<DeviceHub>(), pending)
            })
        })
    {
        operation.status = DeviceStatus::Uncertain;
    }
    Ok(wire::device_state(&operation))
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
        permission::refuse(world, waiter.call, reason);
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
