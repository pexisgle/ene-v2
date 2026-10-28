use super::{Message, RuntimeStatus, Snapshot};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationTarget {
    Configuration(Option<String>),
    Provider(String),
    Providers,
    Models(String),
    Availability(String),
    Credential(String),
    Permission(String),
    Check(String),
    Companion,
    History,
}
#[derive(Clone)]
pub struct RequestContext {
    pub companion: String,
    pub draft: crate::host_model::DraftStamp,
    pub resource: Option<crate::host_model::DraftResource>,
    pub saved_resource: Option<crate::host_model::DraftResource>,
    pub session: crate::infrastructure::host::SessionIdentity,
    pub id: String,
    pub target: OperationTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionPhase {
    Starting = 4,
    Connecting = 1,
    Connected = 2,
    Retrying = 3,
    Stopped = 0,
}
pub enum Command {
    InspectHost,
    Reconnect,
    Send(String),
    Configure {
        settings_revision: u64,
        companion_revision: u64,
        connection_id: Option<String>,
        creation_id: Option<String>,
        provider: String,
        endpoint: String,
        model: String,
        options: ene_rpc::JsonDocument,
        update_connection: bool,
        credential: Option<CredentialEdit>,
    },
    ReloadProviders,
    EnableProvider {
        provider: String,
        enabled: bool,
    },
    Models(String),
    Availability(String),
    Credential(String, String),
    EnvironmentCredential(String),
    RevokeCredential(String),
    RevokePermissions(String),
    Permission {
        consent: ene_rpc::Consent,
        destinations: Vec<String>,
        connection: String,
        conversation: bool,
        check: bool,
    },
    Check(String),
    Companion {
        revision: u64,
        name: String,
        mind: String,
        body: Option<ene_rpc::JsonDocumentPatch>,
        voice: Option<ene_rpc::JsonDocumentPatch>,
    },
    History,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Runtime,
    Conversation,
    Read,
    Write,
    DurableExecution,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Quiet,
    Connection,
    Companion,
    Permission,
    History,
    General,
}
impl Feedback {
    pub const fn operation(self) -> i32 {
        match self {
            Self::Quiet => 0,
            Self::Connection => 1,
            Self::Companion => 2,
            Self::Permission => 3,
            Self::History => 4,
            Self::General => 5,
        }
    }
}
enum DraftAccess {
    Inspect(crate::host_model::DraftResource),
    Save(crate::host_model::DraftResource),
}
impl DraftAccess {
    fn resource(&self) -> crate::host_model::DraftResource {
        match *self {
            Self::Inspect(resource) | Self::Save(resource) => resource,
        }
    }
    fn saved_resource(&self) -> Option<crate::host_model::DraftResource> {
        match *self {
            Self::Inspect(_) => None,
            Self::Save(resource) => Some(resource),
        }
    }
}
pub struct CommandPolicy {
    pub target: Option<OperationTarget>,
    pub admission: Admission,
    pub resource: Option<crate::host_model::DraftResource>,
    pub feedback: Feedback,
    pub saved_resource: Option<crate::host_model::DraftResource>,
}
impl Command {
    pub fn policy(&self) -> CommandPolicy {
        use crate::host_model::DraftResource as Resource;
        let (target, admission, draft, feedback) = match self {
            Self::InspectHost | Self::Reconnect => {
                (None, Admission::Runtime, None, Feedback::Quiet)
            }
            Self::Send(_) => (None, Admission::Conversation, None, Feedback::Quiet),
            Self::Configure { connection_id, .. } => (
                Some(OperationTarget::Configuration(connection_id.clone())),
                Admission::Write,
                Some(DraftAccess::Save(Resource::Connection)),
                Feedback::Connection,
            ),
            Self::ReloadProviders => (
                Some(OperationTarget::Providers),
                Admission::Write,
                None,
                Feedback::General,
            ),
            Self::EnableProvider { provider, .. } => (
                Some(OperationTarget::Provider(provider.clone())),
                Admission::Write,
                None,
                Feedback::General,
            ),
            Self::Models(id) => (
                Some(OperationTarget::Models(id.clone())),
                Admission::Read,
                Some(DraftAccess::Inspect(Resource::Connection)),
                Feedback::General,
            ),
            Self::Availability(id) => (
                Some(OperationTarget::Availability(id.clone())),
                Admission::Read,
                Some(DraftAccess::Inspect(Resource::Permission)),
                Feedback::Quiet,
            ),
            Self::Credential(id, _)
            | Self::EnvironmentCredential(id)
            | Self::RevokeCredential(id) => (
                Some(OperationTarget::Credential(id.clone())),
                Admission::Write,
                Some(DraftAccess::Inspect(Resource::Connection)),
                Feedback::General,
            ),
            Self::Permission { connection, .. } | Self::RevokePermissions(connection) => (
                Some(OperationTarget::Permission(connection.clone())),
                Admission::Write,
                Some(DraftAccess::Save(Resource::Permission)),
                Feedback::Permission,
            ),
            Self::Check(id) => (
                Some(OperationTarget::Check(id.clone())),
                Admission::DurableExecution,
                None,
                Feedback::General,
            ),
            Self::Companion { .. } => (
                Some(OperationTarget::Companion),
                Admission::Write,
                Some(DraftAccess::Save(Resource::Companion)),
                Feedback::Companion,
            ),
            Self::History => (
                Some(OperationTarget::History),
                Admission::Read,
                None,
                Feedback::History,
            ),
        };
        CommandPolicy {
            target,
            admission,
            resource: draft.as_ref().map(DraftAccess::resource),
            saved_resource: draft.as_ref().and_then(DraftAccess::saved_resource),
            feedback,
        }
    }
}

pub enum CredentialEdit {
    Secret(String),
    Environment,
    Revoke,
}
pub struct Request {
    pub companion: String,
    pub command: Command,
    pub context: Option<RequestContext>,
}
impl Request {
    pub fn new(companion: impl Into<String>, command: Command) -> Self {
        Self {
            companion: companion.into(),
            command,
            context: None,
        }
    }
}
pub enum Event {
    Action(Box<ActionResultEnvelope>),
    Session(crate::infrastructure::host::SessionIdentity),
    Runtime(RuntimeStatus),
    RetryIn(i32),
    View(Box<Snapshot>, Vec<Message>),
    Connection(ConnectionPhase, String),
    Accepted(String),
    SendFailed(String),
    Error(String),
    Providers(Vec<ene_rpc::Provider>),
}

pub struct ActionResultEnvelope {
    pub context: RequestContext,
    pub payload: ActionEvent,
}
pub enum ActionEvent {
    Error(String),
    ConnectionSaved(String),
    Done(String, bool, Option<String>),
    Older(super::sync::HistoryApplied),
    Models(String, Vec<ene_rpc::Model>),
    Availability(ene_rpc::Availability),
}
