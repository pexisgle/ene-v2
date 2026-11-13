//! The one boundary between stored domain values and the proto contract.
use super::{
    client::{
        Activation, CapabilityApproval, CapabilitySpec, Client, DeviceOperation, DeviceStatus,
    },
    error::Failure,
    json::JsonDoc,
    mind::memory::Memory,
    mind::{Assignment, Assignments, Companion, Profile, State},
    permission::{Approval, ApprovalStatus, PolicyEffect, PolicyRule},
    settings::{
        Check, Connection, ConnectionConfig, ConnectionKind, CredentialState, Provider, Settings,
    },
    work::{Inquiry, Run, RunState, WorkSession, WorkStatus},
};
use ene_rpc as w;
use serde_json::Value;

pub(crate) fn json_text(value: &Value) -> String {
    value.to_string()
}

pub(crate) fn document(document: &JsonDoc) -> w::JsonDocument {
    w::JsonDocument {
        schema: document.schema.clone(),
        json: json_text(&document.json),
        ..Default::default()
    }
}

pub(crate) fn document_from(value: &w::JsonDocument) -> JsonDoc {
    let json = serde_json::from_str(&value.json).unwrap_or(Value::Null);
    JsonDoc::new(value.schema.clone(), json)
}

pub(crate) fn failure(value: &Failure) -> w::Failure {
    w::Failure {
        code: value.code.clone(),
        message: value.message.clone(),
        ..Default::default()
    }
}

pub(crate) fn failure_from(value: &w::Failure) -> Failure {
    Failure::new(value.code.clone(), value.message.clone())
}

fn assignment(value: &Assignment) -> w::ModelAssignment {
    w::ModelAssignment {
        connection_id: value.connection_id.clone(),
        model_id: value.model_id.clone(),
        ..Default::default()
    }
}

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

pub(crate) fn profile_from(value: &w::MindProfile) -> Profile {
    Profile {
        facets: value.facets.clone(),
        examples: value.examples.clone(),
        background: value.background.clone(),
        expression: value.expression.clone(),
    }
}

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

pub(crate) fn connection_kind(value: ConnectionKind) -> w::ConnectionKind {
    match value {
        ConnectionKind::Llm => w::ConnectionKind::Llm,
        ConnectionKind::Tts => w::ConnectionKind::Tts,
        ConnectionKind::Stt => w::ConnectionKind::Stt,
        ConnectionKind::Embedding => w::ConnectionKind::Embedding,
        ConnectionKind::Mcp => w::ConnectionKind::Mcp,
        ConnectionKind::Acp => w::ConnectionKind::Acp,
    }
}

pub(crate) fn connection(value: &Connection) -> w::Connection {
    let endpoint = value.endpoint();
    let endpoint = || w::ConnectionEndpoint {
        adapter: endpoint.adapter.clone(),
        endpoint: endpoint.endpoint.clone(),
        options_schema: endpoint.options_schema.clone(),
        options: endpoint.options.clone(),
        ..Default::default()
    };
    let config = match &value.config {
        ConnectionConfig::Llm(config) => {
            w::connection_config::Config::Llm(Box::new(w::LlmConnection {
                endpoint: Some(endpoint()).into(),
                model_ids: config.model_ids.clone(),
                ..Default::default()
            }))
        }
        ConnectionConfig::Tts(config) => {
            w::connection_config::Config::Tts(Box::new(w::TtsConnection {
                endpoint: Some(endpoint()).into(),
                voice_ids: config.voice_ids.clone(),
                ..Default::default()
            }))
        }
        ConnectionConfig::Stt(config) => {
            w::connection_config::Config::Stt(Box::new(w::SttConnection {
                endpoint: Some(endpoint()).into(),
                language_hints: config.language_hints.clone(),
                ..Default::default()
            }))
        }
        ConnectionConfig::Embedding(config) => {
            w::connection_config::Config::Embedding(Box::new(w::EmbeddingConnection {
                endpoint: Some(endpoint()).into(),
                model_id: config.model_id.clone(),
                dimensions: config.dimensions,
                ..Default::default()
            }))
        }
        ConnectionConfig::Mcp(config) => {
            w::connection_config::Config::Mcp(Box::new(w::McpConnection {
                endpoint: Some(endpoint()).into(),
                allowed_tools: config.allowed_tools.clone(),
                ..Default::default()
            }))
        }
        ConnectionConfig::Acp(config) => {
            w::connection_config::Config::Acp(Box::new(w::AcpConnection {
                endpoint: Some(endpoint()).into(),
                command: config.command.clone(),
                args: config.args.clone(),
                ..Default::default()
            }))
        }
    };
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

pub(crate) fn approval_status(value: ApprovalStatus) -> w::ApprovalStatus {
    match value {
        ApprovalStatus::Pending => w::ApprovalStatus::Pending,
        ApprovalStatus::Approved => w::ApprovalStatus::Approved,
        ApprovalStatus::Denied => w::ApprovalStatus::Denied,
        ApprovalStatus::Expired => w::ApprovalStatus::Expired,
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
        status: approval_status(value.status).into(),
        created_at: value.created_at.clone(),
        client_id: value.client_id.clone(),
        activation_revision: value.activation_revision,
        capability: value.capability.clone(),
        ..Default::default()
    }
}

pub(crate) fn policy_effect(value: PolicyEffect) -> w::PolicyEffect {
    match value {
        PolicyEffect::Allow => w::PolicyEffect::Allow,
        PolicyEffect::Deny => w::PolicyEffect::Deny,
        PolicyEffect::Confirm => w::PolicyEffect::Confirm,
    }
}

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

pub(crate) fn capability_approval(value: CapabilityApproval) -> w::CapabilityApproval {
    match value {
        CapabilityApproval::Unspecified => w::CapabilityApproval::Unspecified,
        CapabilityApproval::Auto => w::CapabilityApproval::Auto,
        CapabilityApproval::Confirm => w::CapabilityApproval::Confirm,
    }
}

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

pub(crate) fn activation(value: &Activation) -> w::Activation {
    w::Activation {
        companion_id: value.companion_id.clone(),
        client_id: value.client_id.clone(),
        revision: value.revision,
        ..Default::default()
    }
}

pub(crate) fn work_status(value: WorkStatus) -> w::WorkStatus {
    match value {
        WorkStatus::Idle => w::WorkStatus::Idle,
        WorkStatus::Active => w::WorkStatus::Active,
        WorkStatus::Closed => w::WorkStatus::Closed,
    }
}

pub(crate) fn run_state(value: RunState) -> w::RunStatus {
    match value {
        RunState::Starting => w::RunStatus::Starting,
        RunState::Running => w::RunStatus::Running,
        RunState::WaitingInput => w::RunStatus::WaitingInput,
        RunState::Cancelling => w::RunStatus::Cancelling,
        RunState::Completed => w::RunStatus::Completed,
        RunState::Cancelled => w::RunStatus::Cancelled,
        RunState::CancellationUnconfirmed => w::RunStatus::CancellationUnconfirmed,
        RunState::Failed => w::RunStatus::Failed,
    }
}

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

pub(crate) fn run_info(value: &Run) -> w::RunInfo {
    let detail = value.detail();
    w::RunInfo {
        run_id: value.run_id.clone(),
        work_id: value.work_id.clone(),
        state: run_state(value.state).into(),
        failure: detail.failure.map(failure).into(),
        pending_request_id: detail.pending_request_id.map(ToOwned::to_owned),
        cancellation_requested_at: detail.cancellation_requested_at.map(ToOwned::to_owned),
        cancellation_reason: detail.cancellation_reason.map(ToOwned::to_owned),
        unverified_pids: detail.unverified_pids.to_vec(),
        ..Default::default()
    }
}

pub(crate) fn inquiry(value: &Inquiry) -> w::WorkInquiry {
    w::WorkInquiry {
        request_id: value.request_id.clone(),
        prompt: value.prompt.clone(),
        choices: value.choices.clone(),
        ..Default::default()
    }
}

pub(crate) fn work_view(session: &WorkSession, runs: &[Run], inquiries: &[Inquiry]) -> w::WorkView {
    w::WorkView {
        session: Some(work_session(session)).into(),
        runs: runs.iter().map(run_info).collect(),
        pending_inquiries: inquiries.iter().map(inquiry).collect(),
        ..Default::default()
    }
}

pub(crate) fn work_run_finished(value: &Run) -> w::WorkRunFinished {
    let detail = value.detail();
    w::WorkRunFinished {
        state: run_state(value.state).into(),
        failure: detail.failure.map(failure).into(),
        pending_request_id: detail.pending_request_id.map(ToOwned::to_owned),
        cancellation_requested_at: detail.cancellation_requested_at.map(ToOwned::to_owned),
        cancellation_reason: detail.cancellation_reason.map(ToOwned::to_owned),
        unverified_pids: detail.unverified_pids.to_vec(),
        ..Default::default()
    }
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

pub(crate) fn device_status(value: DeviceStatus) -> w::DeviceOperationStatus {
    match value {
        DeviceStatus::Pending => w::DeviceOperationStatus::Pending,
        DeviceStatus::Dispatched => w::DeviceOperationStatus::Dispatched,
        DeviceStatus::Completed => w::DeviceOperationStatus::Completed,
        DeviceStatus::Failed => w::DeviceOperationStatus::Failed,
        DeviceStatus::Uncertain => w::DeviceOperationStatus::Uncertain,
    }
}

pub(crate) fn device_operation(value: &DeviceOperation, subscription: &str) -> w::DeviceOperation {
    w::DeviceOperation {
        operation_id: value.id.clone(),
        capability: value.capability.clone(),
        input: json_text(&value.input),
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
        output: value.output.as_ref().map(json_text).unwrap_or_default(),
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

/// A credential state name for the wire.
pub(crate) fn credential_state(value: CredentialState) -> &'static str {
    value.as_str()
}
