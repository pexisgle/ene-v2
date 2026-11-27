//! Pure client domain models decoupled from Connect RPC / Protobuf wire types.

use std::collections::BTreeMap;

pub mod companion;
pub mod connection;
pub mod policy;

pub use crate::{
    Activation, BodyBinding, COMPUTER_USE, Check as ConnectionCheck,
    Endpoint as ConnectionEndpoint, Failure, ModelAssignment, ModelAssignments, PolicyEffect,
    PolicyRule, PolicyText, RiskLevel, RunState, VoiceBinding, WorkSession, WorkStatus,
};

pub use companion::{
    Companion, CompanionProfilePatch, CompanionTemplate, Message, MessageRole, MindIndexStatus,
    MindProfile, MindState, MindView, NewCompanionSpec, TurnEvent, TurnStatus,
};
pub use connection::{
    AcpConnection, Connection, ConnectionConfig, ConnectionInput, ConnectionKind,
    EmbeddingConnection, LlmConnection, LlmProvider, McpConnection, ModelInfo, PluginInfo,
    SearchConnection, SttConnection, TtsConnection,
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
    /// Capabilities the client declared at registration.
    pub capabilities: Vec<String>,
    /// Declared capabilities the host has since granted.
    pub granted_capabilities: Vec<String>,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
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
    /// Settings and connections, when the host returned them.
    pub settings: Option<Settings>,
    /// Every companion on the host.
    pub companions: Vec<Companion>,
    /// Running sessions keyed by work ID, with their owning companions.
    pub active_work: BTreeMap<String, crate::CompanionId>,
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
    /// A companion was added or changed.
    Companion(Box<Companion>),
    /// A companion and its active binding were removed.
    CompanionRemoved(crate::CompanionId),
    /// A companion's mind changed.
    MindUpdated(crate::CompanionId),
    /// A message was recorded.
    MessageCreated(Box<Message>),
    /// More text arrived for a streaming reply.
    MessageDelta(MessageDelta),
    /// A turn changed state.
    TurnEvent(Box<TurnEvent>),
    /// A work session changed its lifecycle state.
    WorkStatusChanged {
        /// The session whose status changed.
        work_id: String,
        /// The owner identified by the host's notification envelope.
        companion_id: crate::CompanionId,
        /// The session's current status.
        status: WorkStatus,
    },
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
    /// The host boot and sequence that produced this event or snapshot.
    pub cursor: Option<HostCursor>,
    /// The translated event, when the kind maps to one.
    pub event: Option<NotificationEvent>,
}

/// A position in a single host boot's notification history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCursor {
    /// The boot that issued this position.
    pub epoch: String,
    /// Monotonic position within the boot.
    pub seq: u64,
}

impl HostNotification {
    /// Builds an `initial_state` notification from a fetched snapshot.
    pub fn initial_state(snapshot: Snapshot, cursor: Option<HostCursor>) -> Self {
        Self {
            cursor,
            event: Some(NotificationEvent::InitialState(Box::new(snapshot))),
        }
    }

    /// Builds a `companion` notification from an updated companion record.
    pub fn companion(companion: Companion) -> Self {
        Self {
            cursor: None,
            event: Some(NotificationEvent::Companion(Box::new(companion))),
        }
    }
}
