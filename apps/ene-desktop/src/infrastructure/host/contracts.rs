use super::{Message, Page, RuntimeStatus, Snapshot, Value};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationTarget {
    Configuration(Option<String>),
    Provider(String),
    Providers,
    Models(String),
    Credential(String),
    Permission(String),
    Check(String),
    Companion,
    History,
}
#[derive(Clone)]
pub struct RequestContext {
    pub companion: String,
    pub edit_revision: i32,
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
        connection_id: Option<String>,
        creation_id: Option<String>,
        provider: String,
        endpoint: String,
        model: String,
        options: Value,
        update_connection: bool,
        credential: Option<CredentialEdit>,
    },
    ReloadProviders,
    EnableProvider {
        provider: String,
        enabled: bool,
    },
    Models(String),
    Credential(String, String),
    EnvironmentCredential(String),
    RevokeCredential(String),
    RevokePermissions(String),
    Permission {
        connection: String,
        conversation: bool,
        check: bool,
    },
    Check(String),
    Companion {
        name: String,
        mind: String,
    },
    History,
}
impl Command {
    pub(super) fn target(&self) -> Option<OperationTarget> {
        Some(match self {
            Self::InspectHost | Self::Reconnect | Self::Send(_) => return None,
            Self::Configure { connection_id, .. } => {
                OperationTarget::Configuration(connection_id.clone())
            }
            Self::ReloadProviders => OperationTarget::Providers,
            Self::EnableProvider { provider, .. } => OperationTarget::Provider(provider.clone()),
            Self::Models(id) => OperationTarget::Models(id.clone()),
            Self::Credential(id, _)
            | Self::EnvironmentCredential(id)
            | Self::RevokeCredential(id) => OperationTarget::Credential(id.clone()),
            Self::Permission { connection, .. } => OperationTarget::Permission(connection.clone()),
            Self::RevokePermissions(id) => OperationTarget::Permission(id.clone()),
            Self::Check(id) => OperationTarget::Check(id.clone()),
            Self::Companion { .. } => OperationTarget::Companion,
            Self::History => OperationTarget::History,
        })
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
    Action(RequestContext, Box<Event>),
    Runtime(RuntimeStatus),
    RetryIn(i32),
    View(Box<Snapshot>, Vec<Message>),
    Connection(ConnectionPhase, String),
    Accepted(String),
    SendFailed(String),
    Error(String),
    ConnectionSaved(String),
    Done(String, bool, Option<String>),
    Older(Page<Message>),
    Providers(Vec<ene_api_types::provider::ProviderInfo>),
    Models(String, Vec<ene_api_types::provider::Model>),
}
