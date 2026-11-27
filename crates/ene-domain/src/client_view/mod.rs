//! Pure client domain models decoupled from Connect RPC / Protobuf wire types.

pub mod companion;
pub mod connection;
pub mod policy;

pub use crate::{
    Activation, BodyBinding, COMPUTER_USE, Check as ConnectionCheck,
    Endpoint as ConnectionEndpoint, Failure, ModelAssignment, ModelAssignments, PolicyEffect,
    PolicyRule, PolicyText, RiskLevel, RunState, VoiceBinding, WorkSession, WorkStatus,
};

pub use companion::{
    AutoAssignModelPatch, Companion, CompanionProfilePatch, CompanionTemplate, Message,
    MessageRole, MindIndexStatus, MindProfile, MindState, MindView, NewCompanionSpec, TurnEvent,
    TurnStatus,
};
pub use connection::{
    AcpConnection, Connection, ConnectionConfig, ConnectionInput, ConnectionKind,
    EmbeddingConnection, LlmConnection, LlmProvider, McpConnection, ModelInfo, PluginInfo,
    SearchConnection, SettingField, SttConnection, TtsConnection,
};
pub use ene_policy::{ApprovalDecision, ApprovalStatus};
pub use policy::{
    ApprovalRequest, PolicyRulesUpdate, PolicyTextsUpdate, SKIP_FILE_CONFIRMATION_PREFIX,
    SKIP_TOOL_CONFIRMATION_ID, is_file_confirmation_rule, tool_confirmation_update,
};

/// Schema-tagged JSON payload carried by companion bindings, mind state, and events.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonDocument {
    /// Schema id the payload conforms to, such as `ene.body.v1`.
    #[serde(default)]
    pub schema: String,
    /// The payload itself, as JSON text.
    #[serde(default)]
    pub json: String,
}

/// Host lifecycle reported in snapshots and health notifications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostHealth {
    /// The host has yet to report that it can serve requests.
    #[default]
    Starting,
    /// The host is serving requests.
    Ready,
    /// The host must be repaired before it can serve requests.
    RecoveryRequired,
    /// The host is shutting down.
    Stopping,
}

/// Registered client device state known to the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientInfo {
    /// Client-assigned id, unique across registered clients.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Platform the client runs on, as the client reports it.
    pub platform: String,
    /// Capabilities the client declared at registration.
    pub capabilities: Vec<String>,
    /// Declared capabilities the host has since granted.
    pub granted_capabilities: Vec<String>,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Last contact, as the host stamped it.
    pub last_seen: String,
    /// Whether the host currently holds a session to the client.
    pub connected: bool,
    /// Companions currently resident on the client.
    pub active_companions: Vec<String>,
}

/// Detail payload for a work notification event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkEventDetail {
    /// The session moved to a new lifecycle state.
    StatusChanged(WorkStatus),
    /// A run started.
    RunStarted,
    /// A run ended, for whatever reason.
    RunFinished {
        /// The state the run ended in.
        state: Option<RunState>,
        /// Why the run failed, when it did.
        failure: Option<Failure>,
        /// The approval the run stopped on, when it is waiting for one.
        pending_request_id: Option<String>,
    },
}

/// Work session or run lifecycle event emitted by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkEvent {
    /// The session the event is about.
    pub work_id: String,
    /// The run the event is about; empty for a session-level event.
    pub run_id: String,
    /// What happened, when the host described it.
    pub detail: Option<WorkEventDetail>,
}

/// Persisted host settings and configured connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Optimistic-concurrency counter for the settings view.
    pub revision: u64,
    /// Every configured external connection.
    pub connections: Vec<Connection>,
    /// When the view last changed, as the host stamped it.
    pub updated_at: String,
    /// Free-text policies the decision engine reads.
    pub policy_texts: Vec<PolicyText>,
}

/// Authoritative host state snapshot received on subscribe or resnapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Version string the host reported.
    pub server_version: String,
    /// Host readiness, when the host reported it.
    pub health: Option<HostHealth>,
    /// Settings and connections, when the host returned them.
    pub settings: Option<Settings>,
    /// Every companion on the host.
    pub companions: Vec<Companion>,
    /// Work sessions retained on the host.
    pub work_sessions: Vec<WorkSession>,
    /// The companion the host considers active.
    pub active_companion_id: Option<String>,
    /// Approvals waiting on a person.
    pub pending_approvals: Vec<ApprovalRequest>,
    /// Registered clients.
    pub clients: Vec<ClientInfo>,
    /// Saved permission rules.
    pub policy_rules: Vec<PolicyRule>,
    /// Active client bindings per companion.
    pub activations: Vec<Activation>,
}

/// Generic host notification event payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEvent {
    /// Notification kind, in the host's own spelling.
    pub kind: String,
    /// The payload, when the host sent one.
    pub payload: Option<JsonDocument>,
}

/// Notification indicating a companion's mind profile or state changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindUpdated {
    /// The companion whose mind changed.
    pub companion_id: String,
    /// The profile revision after the change.
    pub profile_revision: u64,
    /// The state revision after the change.
    pub state_revision: u64,
    /// When the change happened, as the host stamped it.
    pub updated_at: String,
}

/// Streaming assistant text fragment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDelta {
    /// The companion that is speaking.
    pub companion_id: String,
    /// Stream the fragment belongs to; fragments of one reply share it.
    pub stream_id: String,
    /// The text that arrived.
    pub delta: String,
}

/// Domain representation of host notification events processed by the client reducer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationEvent {
    /// Host readiness changed.
    Health(HostHealth),
    /// A companion was added or changed.
    Companion(Box<Companion>),
    /// A companion and its active binding were removed.
    CompanionRemoved(crate::CompanionId),
    /// A companion's mind changed.
    MindUpdated(MindUpdated),
    /// A message was recorded.
    MessageCreated(Box<Message>),
    /// More text arrived for a streaming reply.
    MessageDelta(MessageDelta),
    /// A turn changed state.
    TurnEvent(Box<TurnEvent>),
    /// A work session or run changed state.
    WorkEvent(WorkEvent),
    /// An approval was raised; `None` when it was withdrawn.
    ApprovalRequested(Option<Box<ApprovalRequest>>),
    /// The saved permission rules changed.
    PolicyRulesUpdated(Vec<PolicyRule>),
    /// The free-text policies changed.
    PolicyTextsUpdated(Vec<PolicyText>),
    /// Settings changed; `None` when the host sent no body.
    SettingsUpdated(Option<Box<Settings>>),
    /// A client registered or changed; `None` when it was removed.
    ClientUpdated(Option<Box<ClientInfo>>),
    /// A notification kind this build has no domain type for.
    Generic(HostEvent),
    /// The host's whole state, as fetched for a new subscription.
    InitialState(Box<Snapshot>),
    /// The client fell too far behind and must refetch.
    ResyncRequired {
        /// Why the host asked for a resync.
        reason: String,
    },
    /// A companion's client binding changed.
    ActivationUpdated(Activation),
    /// A notification kind with nothing worth rendering.
    Other,
}

/// Host notification envelope handed from the session stream or RPC calls to the reducer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostNotification {
    /// Notification kind, in the host's own spelling.
    pub kind: String,
    /// The translated event, when the kind maps to one.
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
