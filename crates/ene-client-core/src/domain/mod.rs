//! Pure client domain models decoupled from Connect RPC / Protobuf wire types.

pub mod companion;
pub mod connection;
pub mod policy;

pub use companion::{
    AutoAssignModelPatch, BodyBinding, Companion, CompanionProfilePatch, CompanionTemplate,
    Message, MessageRole, MindIndexStatus, MindProfile, MindState, MindView, ModelAssignment,
    ModelAssignments, NewCompanionSpec, TurnEvent, TurnStatus, VoiceBinding,
};
pub use connection::{
    AcpConnection, Connection, ConnectionCheck, ConnectionConfig, ConnectionEndpoint,
    ConnectionInput, ConnectionKind, EmbeddingConnection, LlmConnection, LlmProvider,
    McpConnection, ModelInfo, PluginInfo, SearchConnection, SettingField, SttConnection,
    TtsConnection,
};
/// Re-exported from the policy crate so the UI reaches it through the domain
/// module it already imports rather than depending on `ene-policy` directly.
pub use ene_rpc::RiskLevel;
pub use policy::{
    ApprovalDecision, ApprovalRequest, ApprovalStatus, PolicyEffect, PolicyRule, PolicyRulesUpdate,
    PolicyText, PolicyTextsUpdate, SKIP_FILE_CONFIRMATION_PREFIX, SKIP_TOOL_CONFIRMATION_ID,
    is_file_confirmation_rule, tool_confirmation_update,
};

/// Desktop GUI capability executed by a work agent on its bound client.
pub const COMPUTER_USE: &str = "computer.use";

/// Structured host failure payload surfaced on connection checks, turns, and runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

/// Schema-tagged JSON payload carried by companion bindings, mind state, and events.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonDocument {
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub json: String,
}

/// Host readiness summary reported in snapshots and health notifications.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostHealth {
    pub ready: bool,
    pub recovery_required: bool,
    pub stopping: bool,
}

/// Registered client device state known to the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub capabilities: Vec<String>,
    pub granted_capabilities: Vec<String>,
    pub revision: u64,
    pub last_seen: String,
    pub connected: bool,
    pub active_companions: Vec<String>,
}

/// Active client binding for a companion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activation {
    pub companion_id: String,
    pub client_id: Option<String>,
}

/// Lifecycle state of a host work session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkStatus {
    #[default]
    Unspecified,
    Idle,
    Active,
    Closed,
}

/// Execution state of a work run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunStatus {
    #[default]
    Unspecified,
    Starting,
    Running,
    WaitingInput,
    Completed,
    Cancelled,
    Failed,
}

/// Work session summary retained in host snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkSession {
    pub id: String,
    pub companion_id: String,
    pub title: String,
    pub status: WorkStatus,
    pub created_at: String,
    pub updated_at: String,
    pub client_id: String,
}

/// Detail payload for a work notification event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkEventDetail {
    StatusChanged(WorkStatus),
    RunStarted,
    RunFinished {
        state: RunStatus,
        failure: Option<Failure>,
        pending_request_id: Option<String>,
    },
}

/// Work session or run lifecycle event emitted by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkEvent {
    pub work_id: String,
    pub run_id: String,
    pub detail: Option<WorkEventDetail>,
}

/// Persisted host settings and configured connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Connection>,
    pub updated_at: String,
    pub policy_texts: Vec<PolicyText>,
}

/// Authoritative host state snapshot received on subscribe or resnapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub server_version: String,
    pub health: Option<HostHealth>,
    pub settings: Option<Settings>,
    pub companions: Vec<Companion>,
    pub work_sessions: Vec<WorkSession>,
    pub active_companion_id: Option<String>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub clients: Vec<ClientInfo>,
    pub policy_rules: Vec<PolicyRule>,
    pub activations: Vec<Activation>,
}

/// Generic host notification event payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEvent {
    pub kind: String,
    pub payload: Option<JsonDocument>,
}

/// Notification indicating a companion's mind profile or state changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindUpdated {
    pub companion_id: String,
    pub profile_revision: u64,
    pub state_revision: u64,
    pub updated_at: String,
}

/// Streaming assistant text fragment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDelta {
    pub companion_id: String,
    pub stream_id: String,
    pub delta: String,
}

/// Domain representation of host notification events processed by the client reducer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationEvent {
    Health(HostHealth),
    Companion(Box<Companion>),
    MindUpdated(MindUpdated),
    MessageCreated(Box<Message>),
    MessageDelta(MessageDelta),
    TurnEvent(Box<TurnEvent>),
    WorkEvent(WorkEvent),
    ApprovalRequested(Option<Box<ApprovalRequest>>),
    PolicyRulesUpdated(Vec<PolicyRule>),
    PolicyTextsUpdated(Vec<PolicyText>),
    SettingsUpdated(Option<Box<Settings>>),
    ClientUpdated(Option<Box<ClientInfo>>),
    Generic(HostEvent),
    InitialState(Box<Snapshot>),
    ResyncRequired { reason: String },
    ActivationUpdated(Activation),
    Other,
}

/// Host notification envelope handed from the session stream or RPC calls to the reducer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostNotification {
    pub kind: String,
    pub event: Option<NotificationEvent>,
}

impl HostNotification {
    /// Builds an `initial_state` notification from a fetched snapshot.
    pub fn initial_state(snapshot: Snapshot) -> Self {
        Self {
            kind: "initial_state".into(),
            event: Some(NotificationEvent::InitialState(Box::new(snapshot))),
        }
    }

    /// Builds a `settings` notification from an updated settings payload.
    pub fn settings_updated(settings: Settings) -> Self {
        Self {
            kind: "settings".into(),
            event: Some(NotificationEvent::SettingsUpdated(Some(Box::new(settings)))),
        }
    }

    /// Builds a `companion` notification from an updated companion record.
    pub fn companion(companion: Companion) -> Self {
        Self {
            kind: "companion".into(),
            event: Some(NotificationEvent::Companion(Box::new(companion))),
        }
    }
}
