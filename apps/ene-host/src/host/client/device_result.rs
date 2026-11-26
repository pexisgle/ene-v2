//! The client's answer to a device operation.
//!
//! The answer is matched to its operation by id and validated against the
//! declared output schema. A Computer Use answer that can no longer be trusted
//! makes the operation uncertain, never a success.

use super::{
    DeviceOperation,
    computer::{self, ComputerUse},
    device::{get_state, uncertain},
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

/// Records a client's result and answers the waiting tool call. The result is
/// matched to its operation by id; the client id it carries is the record.
pub(crate) fn complete(
    world: &mut World,
    result: &w::DeviceOperationResult,
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
        result,
    ) {
        ComputerUseCheck::Skip => None,
        ComputerUseCheck::Ready(pending) => Some(pending),
        ComputerUseCheck::MarkUncertain => {
            uncertain(
                world,
                &operation.id,
                "the Computer Use operation's result is unknown; do not retry it",
            );
            return get_state(
                &store,
                world.get_resource::<ComputerUse>(),
                world.resource::<DeviceHub>(),
                &operation.id,
            );
        }
    };
    let (base_success, failure, validated) = parse_and_validate_output(&store, &operation, result)?;
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
                world.resource_mut::<ComputerUse>().save_frame(frame);
            }
            let outcome = build_waiter_outcome(&operation, output.as_ref());
            permission::decide(world, waiter.call, outcome);
        } else {
            let reason = failure.as_ref().map_or_else(
                || "the operation failed".into(),
                |failure| failure.message.clone(),
            );
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
    let failure = result.failure.as_option().cloned().map(Into::into);
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
            computer::result(pending, &output)
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
