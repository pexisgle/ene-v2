//! The one boundary between stored domain values and the proto contract.
use super::{
    client::{
        Activation, CapabilityApproval, CapabilitySpec, Client, DeviceOperation, DeviceStatus,
    },
    error::{Failure, HostError},
    json::JsonDoc,
    mind::memory::Memory,
    mind::{Assignment, Assignments, Companion, Profile, State},
    permission::{Approval, ApprovalStatus, PolicyEffect, PolicyRule},
    settings::{
        AcpConnection, Check, Connection, ConnectionConfig, ConnectionKind, CredentialState,
        EmbeddingConnection, Endpoint, LlmConnection, McpConnection, Provider, Settings,
        SttConnection, TtsConnection,
    },
    work::{Inquiry, Run, RunState, WorkSession, WorkStatus},
};
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

/// Maps the connection-config oneof in both directions from one variant list:
/// the domain and proto config structs share their names and their kind-specific
/// fields.
macro_rules! connection_configs {
    ($($variant:ident($config:ident) { $($field:ident),* $(,)? }),+ $(,)?) => {
        fn config_to_wire(
            value: &ConnectionConfig,
            endpoint: &w::ConnectionEndpoint,
        ) -> w::connection_config::Config {
            match value {
                $(ConnectionConfig::$variant(config) => {
                    w::connection_config::Config::$variant(Box::new(w::$config {
                        endpoint: Some(endpoint.clone()).into(),
                        $($field: config.$field.clone(),)*
                        ..Default::default()
                    }))
                }),+
            }
        }

        fn config_from_wire(value: w::connection_config::Config) -> ConnectionConfig {
            match value {
                $(w::connection_config::Config::$variant(config) => {
                    ConnectionConfig::$variant($config {
                        endpoint: endpoint_from(config.endpoint.as_option()),
                        $($field: config.$field,)*
                    })
                }),+
            }
        }
    };
}

connection_configs! {
    Llm(LlmConnection) { model_ids },
    Tts(TtsConnection) { voice_ids },
    Stt(SttConnection) { language_hints },
    Embedding(EmbeddingConnection) { model_id, dimensions },
    Mcp(McpConnection) { allowed_tools },
    Acp(AcpConnection) { command, args },
}

/// Rebuilds the common endpoint from the contract.
pub(crate) fn endpoint_from(value: Option<&w::ConnectionEndpoint>) -> Endpoint {
    value.map_or_else(Endpoint::default, |value| Endpoint {
        adapter: value.adapter.clone(),
        endpoint: value.endpoint.clone(),
        options_schema: value.options_schema.clone(),
        options: value.options.clone(),
    })
}

/// Rebuilds a connection from a settings write.
pub(crate) fn connection_from(input: w::ConnectionInput) -> Result<Connection, HostError> {
    let kind = match input.kind.to_i32() {
        1 => ConnectionKind::Llm,
        2 => ConnectionKind::Tts,
        3 => ConnectionKind::Stt,
        4 => ConnectionKind::Embedding,
        5 => ConnectionKind::Mcp,
        6 => ConnectionKind::Acp,
        _ => return Err(HostError::invalid("connection kind is required")),
    };
    let config = input
        .config
        .into_option()
        .and_then(|config| config.config)
        .map(config_from_wire)
        .ok_or_else(|| HostError::invalid("connection config is required"))?;
    Ok(Connection {
        id: input.id,
        name: input.name,
        enabled: input.enabled,
        kind,
        config,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    })
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

wire_copy!(failure, Failure => w::Failure { code, message });

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

pub(crate) fn failure_from(value: &w::Failure) -> Failure {
    Failure::new(value.code.clone(), value.message.clone())
}

wire_copy!(assignment, Assignment => w::ModelAssignment { connection_id, model_id });

pub(crate) fn assignments(value: &Assignments) -> w::ModelAssignments {
    w::ModelAssignments {
        companion: Some(assignment(&value.companion)).into(),
        task: Some(assignment(&value.task)).into(),
        perception: value.perception.as_ref().map(assignment).into(),
        compaction: value.compaction.as_ref().map(assignment).into(),
        consolidation: value.consolidation.as_ref().map(assignment).into(),
        decision: value.decision.as_ref().map(assignment).into(),
        embedding: value.embedding.as_ref().map(assignment).into(),
        ..Default::default()
    }
}

pub(crate) fn assignments_from(value: &w::ModelAssignments) -> Assignments {
    let read = |field: Option<&w::ModelAssignment>| {
        field.map(|assignment| Assignment {
            connection_id: assignment.connection_id.clone(),
            model_id: assignment.model_id.clone(),
        })
    };
    Assignments {
        companion: read(value.companion.as_option()).unwrap_or_default(),
        task: read(value.task.as_option()).unwrap_or_default(),
        perception: read(value.perception.as_option()),
        compaction: read(value.compaction.as_option()),
        consolidation: read(value.consolidation.as_option()),
        decision: read(value.decision.as_option()),
        embedding: read(value.embedding.as_option()),
    }
}

pub(crate) fn companion(value: &Companion) -> w::Companion {
    w::Companion {
        id: value.id.clone(),
        name: value.name.clone(),
        mind: value.mind.clone(),
        body: Some(document(&value.body)).into(),
        voice: Some(document(&value.voice)).into(),
        assignments: Some(assignments(&value.assignments)).into(),
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
        owner: value.owner.clone(),
        audiences: value.audiences.clone(),
        content: value.content.clone(),
        importance: u32::from(value.importance),
        occurred_at: value.occurred_at.clone(),
        recorded_at: value.recorded_at.clone(),
        last_accessed_at: value.last_accessed_at.clone(),
        revision: value.memory_revision,
        ..Default::default()
    }
}

wire_enum!(connection_kind: ConnectionKind => w::ConnectionKind {
    Llm, Tts, Stt, Embedding, Mcp, Acp
});

pub(crate) fn connection(value: &Connection) -> w::Connection {
    let endpoint = value.endpoint();
    let endpoint = w::ConnectionEndpoint {
        adapter: endpoint.adapter.clone(),
        endpoint: endpoint.endpoint.clone(),
        options_schema: endpoint.options_schema.clone(),
        options: endpoint.options.clone(),
        ..Default::default()
    };
    let config = config_to_wire(&value.config, &endpoint);
    w::Connection {
        id: value.id.clone(),
        name: value.name.clone(),
        enabled: value.enabled,
        credential_state: value.credential_state.as_str().into(),
        revision: value.revision,
        kind: connection_kind(value.kind).into(),
        last_check: value.last_check.as_ref().map(check).into(),
        config: Some(w::ConnectionConfig {
            config: Some(config),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

pub(crate) fn check(value: &Check) -> w::ConnectionCheck {
    w::ConnectionCheck {
        healthy: value.healthy,
        checked_at: value.checked_at.clone(),
        failure: value.failure.as_ref().map(failure).into(),
        capabilities: value.capabilities.clone(),
        resources: value.resources.clone(),
        ..Default::default()
    }
}

pub(crate) fn settings(value: &Settings) -> w::Settings {
    w::Settings {
        revision: value.revision,
        connections: value.connections.iter().map(connection).collect(),
        updated_at: value.updated_at.clone(),
        ..Default::default()
    }
}

pub(crate) fn provider(value: &Provider) -> w::LlmProvider {
    w::LlmProvider {
        id: value.id.clone(),
        name: value.name.clone(),
        adapter: value.adapter.clone(),
        endpoint: value.endpoint.clone(),
        settings: value
            .settings
            .iter()
            .map(|field| w::SettingField {
                key: field.key.clone(),
                label: field.label.clone(),
                kind: field.kind.clone(),
                required: field.required,
                choices: field.choices.clone(),
                ..Default::default()
            })
            .collect(),
        requires_credentials: value.requires_credentials,
        environment_variable: value.environment_variable.clone(),
        ..Default::default()
    }
}

wire_enum!(approval_status: ApprovalStatus => w::ApprovalStatus {
    Pending, Approved, Denied, Expired
});

pub(crate) fn approval(value: &Approval) -> w::ApprovalRequest {
    w::ApprovalRequest {
        request_id: value.request_id.clone(),
        session_id: value.session_id.clone(),
        run_id: value.run_id.clone(),
        action_payload_hash: value.action_payload_hash.clone(),
        tool_name: value.tool_name.clone(),
        normalized_arguments: value.normalized_arguments.clone(),
        status: approval_status(value.status).into(),
        created_at: value.created_at.clone(),
        client_id: value.client_id.clone(),
        activation_revision: value.activation_revision,
        capability: value.capability.clone(),
        ..Default::default()
    }
}

wire_enum!(policy_effect: PolicyEffect => w::PolicyEffect {
    Allow, Deny, Confirm
});

pub(crate) fn policy_rule(value: &PolicyRule) -> w::PolicyRule {
    w::PolicyRule {
        id: value.id.clone(),
        tool_name: value.tool_name.clone(),
        pattern: value.pattern.clone(),
        effect: policy_effect(value.effect).into(),
        created_at: value.created_at.clone(),
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
        approval: match value.approval.to_i32() {
            1 => CapabilityApproval::Auto,
            2 => CapabilityApproval::Confirm,
            _ => CapabilityApproval::Unspecified,
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

wire_copy!(activation, Activation => w::Activation { companion_id, client_id, revision });

wire_enum!(work_status: WorkStatus => w::WorkStatus {
    Idle, Active, Closed
});

wire_enum!(run_state: RunState => w::RunStatus {
    Starting, Running, WaitingInput, Cancelling, Completed, Cancelled,
    CancellationUnconfirmed, Failed
});

pub(crate) fn work_session(value: &WorkSession) -> w::WorkSession {
    w::WorkSession {
        id: value.id.clone(),
        companion_id: value.companion_id.clone(),
        title: value.title.clone(),
        status: work_status(value.status).into(),
        created_at: value.created_at.clone(),
        updated_at: value.updated_at.clone(),
        client_id: value.client_id.clone().unwrap_or_default(),
        activation_revision: value.activation_revision,
        ..Default::default()
    }
}

/// The contract shape of a run's shared detail fields.
macro_rules! run_wire {
    ($type:ident, $value:expr, { $($extra:ident: $extra_value:expr),* $(,)? }) => {{
        let detail = $value.detail();
        w::$type {
            $($extra: $extra_value,)*
            state: run_state($value.state).into(),
            failure: detail.failure.map(failure).into(),
            pending_request_id: detail.pending_request_id.map(ToOwned::to_owned),
            cancellation_requested_at: detail.cancellation_requested_at.map(ToOwned::to_owned),
            cancellation_reason: detail.cancellation_reason.map(ToOwned::to_owned),
            unverified_pids: detail.unverified_pids.to_vec(),
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

wire_copy!(inquiry, Inquiry => w::WorkInquiry { request_id, prompt, choices });

pub(crate) fn work_view(session: &WorkSession, runs: &[Run], inquiries: &[Inquiry]) -> w::WorkView {
    w::WorkView {
        session: Some(work_session(session)).into(),
        runs: runs.iter().map(run_info).collect(),
        pending_inquiries: inquiries.iter().map(inquiry).collect(),
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
        activation_revision: value.activation_revision,
        work_id: value.work_id.clone(),
        ..Default::default()
    }
}

pub(crate) fn device_result(value: &DeviceOperation) -> w::DeviceOperationResult {
    w::DeviceOperationResult {
        client_id: value.client_id.clone(),
        operation_id: value.id.clone(),
        success: value.status == DeviceStatus::Completed,
        output: value
            .output
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        failure: value.failure.as_ref().map(failure).into(),
        activation_revision: value.activation_revision,
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
