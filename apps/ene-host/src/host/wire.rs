//! The one boundary between stored domain values and the proto contract.
use super::{
    client::{CapabilityApproval, CapabilitySpec, Client, DeviceOperation, DeviceStatus},
    mind::{Companion, Profile, State},
    permission::Approval,
    work::{Run, WorkSession},
};
use ene_mind::JsonDoc;
use ene_mind::Memory;
use ene_rpc as w;
use serde_json::Value;

/// Maps an enum whose variants the contract carries under the same names.
macro_rules! wire_enum {
    ($name:ident: $from:ty => $to:ty { $($variant:ident),+ $(,)? }) => {
        pub(crate) fn $name(value: $from) -> $to {
            match value {
                $(<$from>::$variant => <$to>::$variant),+
            }
        }
    };
}

/// Copies the listed fields into the target type, cloning each.
macro_rules! wire_copy {
    ($name:ident, $from:ty => $to:path { $($field:ident),+ $(,)? }) => {
        pub(crate) fn $name(value: &$from) -> $to {
            $to {
                $($field: value.$field.clone(),)+
                ..Default::default()
            }
        }
    };
    (exact $name:ident, $from:ty => $to:path { $($field:ident),+ $(,)? }) => {
        pub(crate) fn $name(value: &$from) -> $to {
            $to {
                $($field: value.$field.clone(),)+
            }
        }
    };
}

pub(crate) fn document(document: &JsonDoc) -> w::JsonDocument {
    w::JsonDocument {
        schema: document.schema.clone(),
        json: document.json.to_string(),
        ..Default::default()
    }
}

pub(crate) fn document_from(value: &w::JsonDocument) -> JsonDoc {
    let json = serde_json::from_str(&value.json).unwrap_or(Value::Null);
    JsonDoc::new(value.schema.clone(), json)
}

pub(crate) fn companion(value: &Companion) -> w::Companion {
    w::Companion {
        id: value.id.clone(),
        name: value.name.clone(),
        mind: value.mind.clone(),
        body: Some(document(&value.body)).into(),
        voice: Some(document(&value.voice)).into(),
        assignments: Some(value.assignments.clone().into()).into(),
        revision: value.revision,
        created_at: value.created_at.clone(),
        updated_at: value.updated_at.clone(),
        persona: value.persona.clone(),
        voice_identifier: value.voice_identifier.clone(),
        ..Default::default()
    }
}

pub(crate) fn mind_profile(value: &Companion) -> w::MindProfile {
    w::MindProfile {
        core: value.mind.clone(),
        revision: value.profile_revision,
        facets: value.profile.facets.clone(),
        examples: value.profile.examples.clone(),
        background: value.profile.background.clone(),
        expression: value.profile.expression.clone(),
        ..Default::default()
    }
}

wire_copy!(exact profile_from, w::MindProfile => Profile { facets, examples, background, expression });

pub(crate) fn mind_state(value: &Companion) -> w::MindState {
    let state: &State = &value.state;
    w::MindState {
        revision: value.state_revision,
        emotion: Some(document(&state.emotion)).into(),
        relationships: Some(document(&state.relationships)).into(),
        tendencies: Some(document(&state.tendencies)).into(),
        concerns: Some(document(&state.concerns)).into(),
        updated_at: value.updated_at.clone(),
        ..Default::default()
    }
}

pub(crate) fn memory(value: &Memory) -> w::MemoryItem {
    w::MemoryItem {
        id: value.id.clone(),
        scope: value.scope.clone(),
        content: value.content.clone(),
        importance: u32::from(value.importance),
        occurred_at: value.occurred_at.clone(),
        recorded_at: value.recorded_at.clone(),
        last_accessed_at: value.last_accessed_at.clone(),
        revision: value.memory_revision,
        ..Default::default()
    }
}

pub(crate) fn approval(value: &Approval) -> w::ApprovalRequest {
    w::ApprovalRequest {
        request_id: value.request_id.clone(),
        session_id: value.session_id.clone(),
        run_id: value.run_id.clone(),
        action_payload_hash: value.action_payload_hash.clone(),
        tool_name: value.tool_name.clone(),
        normalized_arguments: value.normalized_arguments.clone(),
        status: w::ApprovalStatus::from(value.status).into(),
        created_at: value.created_at.clone(),
        client_id: value.client_id.clone(),
        capability: value.capability.clone(),
        external_destination: value.external_destination.clone(),
        ..Default::default()
    }
}

wire_enum!(capability_approval: CapabilityApproval => w::CapabilityApproval {
    Unspecified, Auto, Confirm
});

pub(crate) fn capability_spec(value: &CapabilitySpec) -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        name: value.name.clone(),
        tool_name: value.tool_name.clone(),
        description: value.description.clone(),
        input_schema: value.input_schema.clone(),
        presets: value.presets.clone(),
        approval: capability_approval(value.approval).into(),
        approval_target: value.approval_target.clone(),
        output_schema: value.output_schema.clone(),
        ..Default::default()
    }
}

pub(crate) fn capability_spec_from(value: &w::ClientCapabilitySpec) -> CapabilitySpec {
    CapabilitySpec {
        name: value.name.clone(),
        tool_name: value.tool_name.clone(),
        description: value.description.clone(),
        input_schema: value.input_schema.clone(),
        presets: value.presets.clone(),
        approval: match value.approval.as_known() {
            Some(w::CapabilityApproval::Auto) => CapabilityApproval::Auto,
            Some(w::CapabilityApproval::Confirm) => CapabilityApproval::Confirm,
            Some(w::CapabilityApproval::Unspecified) | None => CapabilityApproval::Unspecified,
        },
        approval_target: value.approval_target.clone(),
        output_schema: value.output_schema.clone(),
    }
}

pub(crate) fn client(value: &Client, connected: bool, active: Vec<String>) -> w::Client {
    w::Client {
        id: value.id.clone(),
        name: value.name.clone(),
        platform: value.platform.clone(),
        capabilities: value.capabilities.clone(),
        granted_capabilities: value.granted_capabilities.clone(),
        revision: value.revision,
        last_seen: value.last_seen.clone(),
        connected,
        active_companions: active,
        capability_specs: value.capability_specs.iter().map(capability_spec).collect(),
        ..Default::default()
    }
}

/// The contract shape of a run's shared detail fields.
macro_rules! run_wire {
    ($type:ident, $value:expr, { $($extra:ident: $extra_value:expr),* $(,)? }) => {{
        w::$type {
            $($extra: $extra_value,)*
            state: w::RunStatus::from($value.state).into(),
            failure: $value.failure.as_ref().map(Into::into).into(),
            pending_request_id: $value.pending_request_id.clone(),
            ..Default::default()
        }
    }};
}

pub(crate) fn run_info(value: &Run) -> w::RunInfo {
    run_wire!(RunInfo, value, {
        run_id: value.run_id.clone(),
        work_id: value.work_id.clone(),
    })
}

pub(crate) fn work_view(session: &WorkSession, runs: &[Run]) -> w::WorkView {
    w::WorkView {
        session: Some(session.into()).into(),
        runs: runs.iter().map(run_info).collect(),
        ..Default::default()
    }
}

pub(crate) fn work_run_finished(value: &Run) -> w::WorkRunFinished {
    run_wire!(WorkRunFinished, value, {})
}

pub(crate) fn work_event(
    session: &WorkSession,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) -> w::WorkEvent {
    w::WorkEvent {
        work_id: session.id.clone(),
        kind: kind.into(),
        detail: Some(detail),
        ..Default::default()
    }
}

wire_enum!(device_status: DeviceStatus => w::DeviceOperationStatus {
    Pending, Dispatched, Completed, Failed, Uncertain
});

pub(crate) fn device_operation(value: &DeviceOperation, subscription: &str) -> w::DeviceOperation {
    w::DeviceOperation {
        operation_id: value.id.clone(),
        capability: value.capability.clone(),
        input: value.input.to_string(),
        subscription_id: subscription.into(),
        client_id: value.client_id.clone(),
        companion_id: value
            .scope
            .companion_id()
            .map_or_default(ToString::to_string),
        work_id: value.scope.work_id().map(str::to_owned),
        run_id: value.scope.run_id().map(str::to_owned),
        ..Default::default()
    }
}

pub(crate) fn device_result(value: &DeviceOperation) -> w::DeviceOperationResult {
    w::DeviceOperationResult {
        client_id: value.client_id.clone(),
        operation_id: value.id.clone(),
        success: value.status == DeviceStatus::Completed,
        output: value.output.as_ref().map_or_default(ToString::to_string),
        failure: value.failure.as_ref().map(Into::into).into(),
        ..Default::default()
    }
}

pub(crate) fn device_state(value: &DeviceOperation) -> w::DeviceOperationState {
    w::DeviceOperationState {
        operation_id: value.id.clone(),
        client_id: value.client_id.clone(),
        status: device_status(value.status).into(),
        result: (value.status == DeviceStatus::Completed || value.status == DeviceStatus::Failed)
            .then(|| device_result(value))
            .into(),
        ..Default::default()
    }
}
