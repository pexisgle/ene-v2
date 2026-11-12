//! The one boundary between stored domain values and the proto contract.
use super::{
    client::{Client, DeviceOperation},
    companion::{Assignments, Companion, ModelAssignment},
    db::parse_json,
    error::Result,
    failure::Failure,
    settings::Connection,
    work::WorkSession,
};
use ene_rpc as w;
use serde_json::Value;

pub(crate) fn json(value: &Value) -> String {
    value.to_string()
}

pub(crate) fn parse(text: &str) -> Result<Value> {
    parse_json(text)
}

pub(crate) fn assignment(value: &ModelAssignment) -> w::ModelAssignment {
    w::ModelAssignment {
        connection_id: value.connection.clone(),
        model_id: value.model.clone(),
        ..Default::default()
    }
}

pub(crate) fn assignments(value: &Assignments) -> w::ModelAssignments {
    w::ModelAssignments {
        companion: value.companion.as_ref().map(assignment).into(),
        task: value.task.as_ref().map(assignment).into(),
        perception: value.perception.as_ref().map(assignment).into(),
        compaction: value.compaction.as_ref().map(assignment).into(),
        consolidation: value.consolidation.as_ref().map(assignment).into(),
        decision: value.decision.as_ref().map(assignment).into(),
        embedding: value.embedding.as_ref().map(assignment).into(),
        ..Default::default()
    }
}

pub(crate) fn companion(value: &Companion) -> w::Companion {
    w::Companion {
        id: value.id.clone(),
        name: value.name.clone(),
        mind: value.mind.clone(),
        body: json(&value.body),
        voice: json(&value.voice),
        assignments: assignments(&value.assignments).into(),
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

pub(crate) fn mind_state(value: &Companion) -> w::MindState {
    w::MindState {
        revision: value.state_revision,
        emotion: json(&value.state.emotion),
        relationships: json(&Value::Array(value.state.relationships.clone())),
        tendencies: json(&Value::from(value.state.tendencies.clone())),
        concerns: json(&Value::from(value.state.concerns.clone())),
        updated_at: value.updated_at.clone(),
        ..Default::default()
    }
}

pub(crate) fn connection(value: &Connection) -> w::Connection {
    w::Connection {
        id: value.id.clone(),
        kind: value.kind.clone(),
        adapter: value.adapter.clone(),
        endpoint: value.endpoint.clone(),
        options: json(&value.options),
        credential_state: value.credential_state.as_str().into(),
        last_check: json(&value.last_check),
        enabled: value.enabled,
        ..Default::default()
    }
}

pub(crate) fn work_session(value: &WorkSession) -> w::WorkSession {
    w::WorkSession {
        id: value.id.clone(),
        companion_id: value.companion_id.clone(),
        title: value.title.clone(),
        status: value.status.as_str().into(),
        created_at: value.created_at.clone(),
        updated_at: value.updated_at.clone(),
        ..Default::default()
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
        ..Default::default()
    }
}

pub(crate) fn failure(value: &Failure) -> w::Failure {
    w::Failure {
        code: value.code.clone(),
        message: value.message.clone(),
        ..Default::default()
    }
}

pub(crate) fn device_task(value: &DeviceOperation, subscription: &str) -> w::DeviceOperation {
    w::DeviceOperation {
        operation_id: value.id.clone(),
        capability: value.capability.clone(),
        input: json(&value.input),
        subscription_id: subscription.into(),
        ..Default::default()
    }
}

pub(crate) fn device_state(value: &DeviceOperation) -> w::DeviceOperationState {
    w::DeviceOperationState {
        operation_id: value.id.clone(),
        client_id: value.client_id.clone(),
        status: value.status.as_str().into(),
        result: value
            .result
            .as_ref()
            .map(|result| w::DeviceOperationResult {
                client_id: value.client_id.clone(),
                operation_id: value.id.clone(),
                success: result.success,
                output: json(&result.output),
                failure: result.failure.as_ref().map(failure).into(),
                ..Default::default()
            })
            .into(),
        ..Default::default()
    }
}

pub(crate) fn memory_item(value: &super::memory::Memory) -> w::MemoryItem {
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
