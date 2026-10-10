//! Shared storage and application values. Opaque fields retain their existing JSON format.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Companion,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Streaming,
    Completed,
    Failed,
    Cancelled,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub companion_id: String,
    pub role: Role,
    pub text: String,
    pub status: MessageStatus,
    pub created_at: String,
    pub sequence: u64,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub in_reply_to: Option<String>,
    pub task_id: Option<String>,
    pub question_id: Option<String>,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Companion {
    pub id: String,
    pub name: String,
    pub body: Option<Value>,
    pub voice: Option<Value>,
    pub mind: String,
    pub model_assignments: ModelAssignments,
    pub availability: Value,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelAssignments {
    pub companion: Option<ModelAssignment>,
    pub task: Option<ModelAssignment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Connection>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub host_id: String,
    pub environment_id: String,
    pub capabilities: Value,
    pub cursor: Cursor,
    pub companions: Vec<Companion>,
    pub messages: Vec<Message>,
    pub message_cursors: BTreeMap<String, Option<String>>,
    pub tasks: Vec<Value>,
    pub questions: Vec<Value>,
    pub settings: Settings,
    pub grants: Vec<DataTransferGrant>,
    #[serde(default)]
    pub health: HostHealth,
}

impl Snapshot {
    /// Preserve active replies and complete text; trim history only through cursors.
    pub fn bound_response(&mut self) -> Result<(), serde_json::Error> {
        use crate::budget::{MAX_RESPONSE_BYTES, encoded_len};
        let essential: std::collections::HashSet<_> = self
            .messages
            .iter()
            .filter(|m| matches!(m.status, MessageStatus::Queued | MessageStatus::Streaming))
            .flat_map(|m| std::iter::once(m.id.clone()).chain(m.in_reply_to.clone()))
            .collect();
        while encoded_len(self, MAX_RESPONSE_BYTES).is_err() {
            let Some((index, _)) = self
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| !essential.contains(&m.id))
                .min_by_key(|(_, m)| m.sequence)
            else {
                return encoded_len(self, MAX_RESPONSE_BYTES).map(|_| ());
            };
            let removed = self.messages.remove(index);
            let sequence = removed
                .sequence
                .checked_add(1)
                .filter(|sequence| *sequence <= i64::MAX as u64)
                .ok_or_else(|| serde_json::Error::io(std::io::Error::other("resource_limit")))?;
            let cursor = format!("{}:{}", sequence, removed.companion_id);
            let current = self
                .message_cursors
                .entry(removed.companion_id)
                .or_default();
            let old = current
                .as_deref()
                .and_then(|cursor| cursor.split_once(':'))
                .and_then(|(seq, _)| seq.parse::<u64>().ok());
            if old.is_none_or(|seq| seq <= removed.sequence) {
                *current = Some(cursor);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HostHealth {
    /// Missing on older hosts. Clients must not infer health from grants.
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub recovery_required: bool,
    #[serde(default)]
    pub stopping: bool,
    #[serde(default)]
    pub credential_cleanup_pending: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationState {
    Granted,
    Missing,
    Partial,
    Invalidated,
    Unavailable,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    Ready,
    ModelUnset,
    CredentialMissing,
    ProviderUnavailable,
    RecoveryRequired,
    Stopping,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OperationAvailability {
    pub operation: String,
    pub purpose: String,
    pub data_kinds: Vec<String>,
    pub authorization: AuthorizationState,
    pub dependency: DependencyState,
    pub busy: bool,
    pub runnable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConnectionAvailability {
    pub companion_id: String,
    pub connection_id: String,
    pub settings_revision: u64,
    pub provider_revision: Option<String>,
    pub destinations: Vec<String>,
    pub connection_fingerprint: Value,
    pub operations: Vec<OperationAvailability>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub epoch: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub companion_id: Option<String>,
    pub data: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}

pub use crate::sync::{Replica, ResyncReason};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Connection {
    pub id: String,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: Value,
    pub credential_state: String,
    pub capabilities: Option<Value>,
    pub last_check: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataTransferGrant {
    pub id: String,
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
    pub status: String,
    pub revision: u64,
    pub invalidation_reason: Option<String>,
    pub connection_fingerprint: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MessageAcceptance {
    pub request_id: String,
    pub message_id: String,
    pub response_message_id: String,
}
/// Mutation inputs shared by the API boundary and application operations.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionInput {
    pub id: Option<String>,
    /// Explicitly create a caller-selected UUID, retained across uncertain retries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub create: bool,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub revision: u64,
    pub connections: Vec<ConnectionInput>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionPatch {
    pub revision: u64,
    pub name: Option<String>,
    pub mind: Option<String>,
    #[serde(default, deserialize_with = "assignment_input")]
    pub model_assignments: Option<BTreeMap<String, Option<ModelAssignment>>>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantInput {
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
    pub consent: GrantConsent,
}
/// Versions of the configuration and implementation presented for consent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantConsent {
    pub settings_revision: u64,
    pub provider_revision: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPatch {
    pub revision: u64,
    pub destinations: Option<Vec<String>>,
    pub data_kinds: Option<Vec<String>>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub text: String,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub question_id: Option<String>,
    pub decision: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub companion_id: String,
    pub purpose: String,
    pub model_id: Option<String>,
}

/// Strict mutation reference; response model references permit additive fields.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAssignmentInput {
    pub connection_id: String,
    pub model_id: String,
}

// Requests remain strict even though model references in responses are extensible.
fn assignment_input<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<BTreeMap<String, Option<ModelAssignment>>>, D::Error> {
    let values = Option::<BTreeMap<String, Option<ModelAssignmentInput>>>::deserialize(d)?;
    Ok(values.map(|values| {
        values
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    value.map(|a| ModelAssignment {
                        connection_id: a.connection_id,
                        model_id: a.model_id,
                    }),
                )
            })
            .collect()
    }))
}

/// Provider-independent model reference. IDs are opaque and must be URL-encoded.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelAssignment {
    pub connection_id: String,
    pub model_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialInput {
    pub secret: Option<String>,
    pub source: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialResult {
    pub credential_state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckResult {
    pub request_id: String,
    pub capabilities: Value,
    pub success: bool,
    pub model_id: String,
    pub content_chunks: u64,
    pub unicode_scalars: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: Failure,
    pub request_id: String,
}
/// Initial WebSocket frame; obtain a snapshot for this live subscription.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrantResult {
    pub grant: DataTransferGrant,
    pub questions: Vec<Value>,
}
/// Text delta offsets are Unicode scalar counts, never UTF-8 byte offsets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageDelta {
    pub message_id: String,
    pub offset: u64,
    pub delta: String,
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use crate::provider::{Capabilities, MetadataScope, SettingField, SettingKind};
    use serde_json::json;
    #[test]
    fn additive_response_fields_and_unknown_kinds_preserve_known_data() {
        let assignment: ModelAssignment = serde_json::from_value(
            json!({"connection_id":"c","model_id":"m","future_option":true}),
        )
        .unwrap();
        assert_eq!(assignment.model_id, "m");
        let capabilities: Capabilities = serde_json::from_value(
            json!({"streaming_text":true,"model_list":false,"future_capability":true}),
        )
        .unwrap();
        assert!(capabilities.streaming_text);
        let field: SettingField = serde_json::from_value(
            json!({"key":"new","label":"New","kind":"future_kind","future_setting":true}),
        )
        .unwrap();
        assert!(matches!(field.kind,SettingKind::Unknown(ref kind) if kind=="future_kind"));
        assert_eq!(serde_json::to_value(field).unwrap()["kind"], "future_kind");
        let status: MessageStatus = serde_json::from_value(json!("future_status")).unwrap();
        assert!(matches!(status, MessageStatus::Unknown(_)));
        assert_eq!(serde_json::to_value(status).unwrap(), "future_status");
        assert!(matches!(
            serde_json::from_value::<MetadataScope>(json!("future_scope")).unwrap(),
            MetadataScope::Unknown(_)
        ));
    }
    #[test]
    fn model_assignment_request_stays_strict_and_omission_keeps_patch_semantics() {
        assert!(serde_json::from_value::<CompanionPatch>(json!({"revision":1,"model_assignments":{"companion":{"connection_id":"c","model_id":"m","future_option":true}}})).is_err());
        let omitted: CompanionPatch = serde_json::from_value(json!({"revision":1})).unwrap();
        assert!(omitted.model_assignments.is_none());
        let cleared: CompanionPatch =
            serde_json::from_value(json!({"revision":1,"model_assignments":{"companion":null}}))
                .unwrap();
        assert_eq!(cleared.model_assignments.unwrap()["companion"], None);
    }
}
