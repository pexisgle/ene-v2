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
    client::{CapabilitySpec, DeviceOperation, DeviceStatus},
    db::{Result, Store},
    error::{Failure, HostError},
    notify::Notifier,
    permission::{self, CallBinding},
    resources::{CompanionAgent, DeviceHub, DeviceTimeout, Waiting, WaitingOperation, WorkAgent},
    util::now,
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
    let _ = dispatch(&mut ctx, &operation, None, Some(companion_id))?;
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
        return Ok(session.companion_id);
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
            let _ = dispatch(&mut ctx, &operation, None, plan.active_companion.as_deref());
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

/// Records a client's result and answers the waiting tool call. The result is
/// matched to its operation by id; the client id it carries is the record.
pub(crate) fn complete(
    world: &mut World,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let Some(operation) = store.client_repo().operation(&result.operation_id)? else {
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
    let computer = match prepare_computer_use(
        world.get_resource::<ComputerUse>(),
        &store,
        world.get_resource::<DeviceHub>(),
        &operation,
        &result,
    ) {
        ComputerUseCheck::Skip => None,
        ComputerUseCheck::Ready(pending) => Some(pending),
        ComputerUseCheck::MarkUncertain => {
            uncertain(
                world,
                &operation.id,
                "the Computer Use operation's result is unknown; do not retry it",
            );
            return get(
                world,
                w::Id {
                    id: operation.id.clone(),
                    ..Default::default()
                },
            );
        }
    };
    let (base_success, failure, validated) =
        parse_and_validate_output(&store, &operation, &result)?;
    let (validated, frame) = apply_computer_use_output(
        world.get_resource_mut::<ComputerUse>().as_deref_mut(),
        &operation,
        computer,
        base_success,
        validated,
    );
    let (success, output, failure) = resolve_operation_outcome(base_success, validated, failure);
    let updated = store
        .client_repo()
        .complete_operation(&operation.id, success, output.as_ref(), failure.as_ref())?
        .ok_or_else(|| HostError::Storage(("device operation update did not persist").into()))?;
    if let Some(waiter) = world
        .resource_mut::<Waiting>()
        .by_operation
        .remove(&operation.id)
    {
        if success {
            if let Some(frame) = frame {
                computer::save_frame(world, frame);
            }
            let outcome = build_waiter_outcome(&operation, output.as_ref());
            permission::decide(world, waiter.call, outcome);
        } else {
            let reason = failure
                .as_ref()
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "the operation failed".into());
            permission::refuse(world, waiter.call, &reason);
        }
    }
    world.resource_mut::<Notifier>().push(
        None,
        Some(&operation.client_id),
        w::notification::Event::DeviceOperationResult(Box::new(wire::device_result(&updated))),
    );
    Ok(wire::device_state(&updated))
}

enum ComputerUseCheck {
    Skip,
    Ready(computer::Pending),
    MarkUncertain,
}

fn prepare_computer_use(
    computer_use: Option<&ComputerUse>,
    store: &Store,
    hub: Option<&DeviceHub>,
    operation: &DeviceOperation,
    result: &w::DeviceOperationResult,
) -> ComputerUseCheck {
    if operation.capability != w::COMPUTER_USE {
        return ComputerUseCheck::Skip;
    }
    let pending = computer_use.and_then(|state| state.operations.get(&operation.id).cloned());
    if !pending
        .as_ref()
        .is_some_and(|pending| hub.is_some_and(|hub| computer::active(store, hub, pending)))
        || result
            .failure
            .as_option()
            .is_some_and(|failure| failure.code == "operation_uncertain")
    {
        return ComputerUseCheck::MarkUncertain;
    }
    match pending {
        Some(pending) => ComputerUseCheck::Ready(pending),
        None => ComputerUseCheck::MarkUncertain,
    }
}

#[derive(Debug, thiserror::Error)]
enum OutputError {
    #[error("{0}")]
    Invalid(#[from] HostError),
    #[error(transparent)]
    ComputerResult(#[from] computer::ResultError),
    #[error("the client returned an invalid Computer Use result")]
    InvalidComputerUseResult,
}

type ParsedOutput = (
    bool,
    Option<Failure>,
    Option<std::result::Result<Value, OutputError>>,
);

fn parse_and_validate_output(
    store: &Store,
    operation: &DeviceOperation,
    result: &w::DeviceOperationResult,
) -> Result<ParsedOutput> {
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
        .client_repo()
        .client(&operation.client_id)?
        .and_then(|client| client.spec(&operation.capability).cloned());
    let validated = output.map(|output| match spec {
        Some(spec) => super::validate_output(&spec, &output)
            .map(|()| output)
            .map_err(OutputError::from),
        None => Ok(output),
    });
    Ok((success, failure, validated))
}

fn apply_computer_use_output(
    computer_use: Option<&mut ComputerUse>,
    operation: &DeviceOperation,
    computer: Option<computer::Pending>,
    success: bool,
    validated: Option<std::result::Result<Value, OutputError>>,
) -> (
    Option<std::result::Result<Value, OutputError>>,
    Option<computer::Frame>,
) {
    let Some(pending) = computer else {
        return (validated, None);
    };
    if let Some(computer_use) = computer_use {
        computer_use.operations.remove(&operation.id);
    }
    let mut frame = None;
    let validated = match (success, validated) {
        (true, Some(Ok(output))) => Some(
            computer::result(pending, output)
                .map(|(metadata, captured)| {
                    frame = captured;
                    metadata
                })
                .map_err(OutputError::from),
        ),
        (_, Some(Err(_))) => Some(Err(OutputError::InvalidComputerUseResult)),
        (_, validated) => validated,
    };
    (validated, frame)
}

fn resolve_operation_outcome(
    success: bool,
    validated: Option<std::result::Result<Value, OutputError>>,
    failure: Option<Failure>,
) -> (bool, Option<Value>, Option<Failure>) {
    match (success, validated) {
        (true, Some(Ok(output))) => (true, Some(output), failure),
        (true, Some(Err(error))) => (
            false,
            None,
            Some(Failure::new("output_invalid", error.to_string())),
        ),
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
    }
}

fn build_waiter_outcome(operation: &DeviceOperation, output: Option<&Value>) -> Outcome {
    if let Some(image) = read_image(&operation.capability, output) {
        use rig_core::{
            message::ToolResultContent,
            tool::{ToolOutput, ToolResult},
        };
        let blocks = vec![
            ToolResultContent::text(
                json!({
                    "operation_id": operation.id,
                    "path": output.and_then(|out| out["path"].as_str()),
                })
                .to_string(),
            ),
            ToolResultContent::Image(image),
        ];
        match ToolOutput::content(blocks) {
            Ok(tool_output) => Outcome::from(ToolResult::success(tool_output)),
            Err(error) => Outcome::refused(error.to_string()),
        }
    } else {
        Outcome::from(rig_core::tool::ToolResult::success(
            json!({"operation_id": operation.id, "output": output})
                .to_string()
                .into(),
        ))
    }
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
pub(crate) fn get(world: &mut World, request: w::Id) -> Result<w::DeviceOperationState> {
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
        let _ = store
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
