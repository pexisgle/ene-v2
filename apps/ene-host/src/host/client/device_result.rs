//! The client's answer to a device operation.
//!
//! The answer is matched to its operation by id and validated against the
//! declared output schema. A Computer Use answer that can no longer be trusted
//! makes the operation uncertain, never a success.

use super::{
    DeviceOperation,
    computer::{self, ComputerUse},
    device::{get_state, uncertain},
    device_dispatch::scope_is_live,
};
use crate::host::{
    db::{Result, Store},
    error::{Failure, HostError},
    notify::Notifier,
    permission,
    resources::{DeviceHub, Waiting},
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::{Value, json};

/// Records a result from the client to which the operation was dispatched.
pub(crate) fn complete(
    world: &mut World,
    result: &w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    let store = world.resource::<Store>().clone();
    let Some((operation, subscription)) = store
        .client_repo()
        .operation_with_subscription(&result.operation_id)?
    else {
        return Err(HostError::NotFound(("device operation not found").into()));
    };
    if operation.client_id != result.client_id {
        return Err(HostError::Permission(
            ("the result belongs to another client").into(),
        ));
    }
    if operation.status.is_terminal() {
        return Ok(wire::device_state(&operation));
    }
    if operation.status != super::DeviceStatus::Dispatched {
        return Err(HostError::Conflict(
            "the operation has not been dispatched".into(),
        ));
    }
    let hub = world.resource::<DeviceHub>();
    let current = hub.subscription(&operation.client_id).filter(|current| {
        subscription
            .as_deref()
            .is_some_and(|saved| current.matches_wire(saved))
    });
    if let Some(current) = current
        && result
            .subscription_id
            .as_deref()
            .is_some_and(|reported| !current.matches_wire(reported))
    {
        return Err(HostError::Conflict(
            "the result belongs to another device stream".into(),
        ));
    }
    let delivery =
        if current.is_some() && scope_is_live(&store, &operation.scope, &operation.client_id)? {
            prepare_computer_use(
                world.get_resource::<ComputerUse>(),
                &store,
                Some(hub),
                &operation,
                result,
            )
        } else {
            ComputerUseCheck::MarkUncertain
        };
    let computer = match delivery {
        ComputerUseCheck::Skip => None,
        ComputerUseCheck::Ready(pending) => Some(pending),
        ComputerUseCheck::MarkUncertain => {
            uncertain(
                world,
                &operation.id,
                "the device operation's result is unknown; do not retry it",
            );
            return get_state(
                &store,
                world.get_resource::<ComputerUse>(),
                world.resource::<DeviceHub>(),
                &operation.id,
            );
        }
    };
    let client = store.client_repo().client(&operation.client_id)?;
    if computer.is_some() {
        world
            .resource_mut::<ComputerUse>()
            .operations
            .remove(&operation.id);
    }
    let outcome = match (result.success, result.failure.as_option().cloned()) {
        (_, Some(failure)) => Err(failure.into()),
        (false, None) => Err(Failure::new("operation_failed", "the operation failed")),
        (true, None) => validate_output(
            client
                .as_ref()
                .and_then(|client| client.spec(&operation.capability)),
            &result.output,
            computer,
        ),
    };
    let persisted = outcome.as_ref().map(|(output, _)| output);
    let updated = store
        .client_repo()
        .complete_operation(&operation.id, persisted)?
        .ok_or_else(|| HostError::Storage("device operation update did not persist".into()))?;
    if let Some(waiter) = world
        .resource_mut::<Waiting>()
        .by_operation
        .remove(&operation.id)
    {
        match outcome {
            Ok((output, frame)) => {
                if let Some(frame) = frame {
                    world.resource_mut::<ComputerUse>().save_frame(frame);
                }
                permission::decide(
                    world,
                    waiter.call,
                    build_waiter_outcome(&operation, &output),
                );
            }
            Err(failure) => permission::refuse(world, waiter.call, &failure.message),
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
    let pending = computer_use.and_then(|state| state.operations.get(&operation.id));
    match pending {
        Some(pending)
            if hub.is_some_and(|hub| computer::active(store, hub, pending))
                && result
                    .failure
                    .as_option()
                    .is_none_or(|failure| failure.code != "operation_uncertain") =>
        {
            ComputerUseCheck::Ready(pending.clone())
        }
        _ => ComputerUseCheck::MarkUncertain,
    }
}

fn validate_output(
    spec: Option<&super::CapabilitySpec>,
    output: &str,
    computer: Option<computer::Pending>,
) -> std::result::Result<(Value, Option<computer::Frame>), Failure> {
    // A capability without an output schema may answer with plain text or an empty body.
    let output = if output.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_owned()))
    };
    if let Some(spec) = spec
        && let Err(error) = super::validate_output(spec, &output)
    {
        let message = if computer.is_some() {
            "the client returned an invalid Computer Use result".into()
        } else {
            error.to_string()
        };
        return Err(Failure::new("output_invalid", message));
    }
    match computer {
        Some(pending) => computer::result(pending, &output)
            .map_err(|error| Failure::new("output_invalid", error.to_string())),
        None => Ok((output, None)),
    }
}

fn build_waiter_outcome(operation: &DeviceOperation, output: &Value) -> Outcome {
    if let Some(image) = read_image(&operation.capability, output) {
        use rig_core::{
            message::ToolResultContent,
            tool::{ToolOutput, ToolResult},
        };
        let blocks = vec![
            ToolResultContent::text(
                json!({
                    "operation_id": operation.id,
                    "path": output["path"].as_str(),
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

fn read_image(capability: &str, output: &Value) -> Option<rig_core::message::Image> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use rig_core::message::{DocumentSourceKind, Image, ImageMediaType};
    if capability != "work.read" {
        return None;
    }
    let encoded = output.get("image_base64")?.as_str()?;
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
