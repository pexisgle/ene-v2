//! Plugin package v1 and Remoc worker control v1. Extism ABI remains owned by Extism.
pub use ene_api_types::plugin::{Extension, Runtime, Service, Target};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
pub const PACKAGE_VERSION: u32 = 1;
pub const API_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_CHUNK: usize = 128 * 1024;
pub const MAX_EVENTS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package_version: u32,
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub entrypoints: Vec<EntryPoint>,
    /// All executable modules, including each entrypoint; assets have a separate budget.
    pub code_files: Vec<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub id: String,
    pub target: Target,
    pub runtime: Runtime,
    /// Relative file inside the verified code snapshot; not a host path.
    pub file: String,
    #[serde(default)]
    pub command: Vec<String>,
    pub provides: Vec<Extension>,
    #[serde(default)]
    pub uses: Vec<Service>,
    #[serde(default)]
    pub provider: Option<ProviderConfiguration>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfiguration {
    pub id: String,
    pub endpoint: String,
    #[serde(default = "object")]
    pub options: Value,
    #[serde(default)]
    pub settings: Vec<crate::provider::SettingField>,
    pub auth: crate::provider::AuthProfile,
    #[serde(default)]
    pub routing: crate::provider::BrokerRouting,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub environment_variable: Option<String>,
    pub destinations: Vec<String>,
    #[serde(default)]
    pub models: Vec<crate::provider::Model>,
}
impl ProviderConfiguration {
    pub fn legacy(&self, package: &Manifest, command: Vec<String>) -> crate::provider::Manifest {
        crate::provider::Manifest {
            protocol_version: crate::provider::VERSION,
            id: self.id.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            command,
            endpoint: self.endpoint.clone(),
            options: self.options.clone(),
            settings: self.settings.clone(),
            auth: self.auth.clone(),
            routing: self.routing.clone(),
            headers: self.headers.clone(),
            environment_variable: self.environment_variable.clone(),
            destinations: self.destinations.clone(),
            models: self.models.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerationRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub preamble: String,
    pub messages: Vec<crate::provider::ChatMessage>,
    pub max_tokens: u64,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmCapabilities {
    pub streaming_text: bool,
    pub model_list: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub cancellation: bool,
}
fn object() -> Value {
    serde_json::json!({})
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub request_id: String,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ListModels { endpoint: String, options: Value },
    Generate { request: GenerationRequest },
    Character { event: CharacterEvent },
    Harness { input: HarnessInput },
    Speech { input: SpeechInput },
}
impl Operation {
    pub fn extension(&self) -> Extension {
        match self {
            Self::ListModels { .. } | Self::Generate { .. } => Extension::Llm,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
            Self::Speech {
                input: SpeechInput::Transcribe { .. },
            } => Extension::Stt,
            Self::Speech { .. } => Extension::Tts,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterEvent {
    Activated,
    Deactivated,
    ConversationStarted,
    ConversationFinished { text: String },
    TaskChanged { task_id: String, state: String },
    Timer { id: String },
    Custom { name: String, data: Value },
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterAction {
    Speak { text: String },
    Expression { name: String, duration_ms: u32 },
    Motion { asset: String, duration_ms: u32 },
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessInput {
    Start { instruction: String },
    Answer { question_id: String, answer: String },
    Result { operation_id: String, result: Value },
    Cancel,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessEvent {
    Progress {
        text: String,
    },
    ModelRequest {
        operation_id: String,
        request: GenerationRequest,
    },
    ToolRequest {
        operation_id: String,
        name: String,
        arguments: Value,
    },
    Question {
        question_id: String,
        text: String,
    },
    Completed {
        report: String,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpeechInput {
    Synthesize {
        utterance_id: String,
        #[serde(default)]
        generation: u64,
        text: String,
        voice: String,
    },
    Transcribe {
        utterance_id: String,
        audio: AudioChunk,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioChunk {
    pub utterance_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub format: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub data_base64: String,
    pub done: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Delta {
        text: String,
    },
    Models {
        models: Vec<crate::provider::Model>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Finished {
        reason: String,
    },
    Character {
        action: CharacterAction,
    },
    Harness {
        event: HarnessEvent,
    },
    Audio {
        chunk: AudioChunk,
    },
    Transcript {
        text: String,
    },
}
impl Event {
    pub fn extension(&self) -> Extension {
        match self {
            Self::Delta { .. }
            | Self::Models { .. }
            | Self::ToolCall { .. }
            | Self::Finished { .. } => Extension::Llm,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
            Self::Audio { .. } => Extension::Tts,
            Self::Transcript { .. } => Extension::Stt,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_base64: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpChunk {
    pub handle: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub data_base64: String,
    pub done: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Log { message: String },
    Configuration,
    StateRead { key: String },
    StateWrite { key: String, value: Value },
    HttpOpen { request: HttpRequest },
    HttpRead { handle: String },
    HttpClose { handle: String },
    Models { request: GenerationRequest },
    Tool { name: String, arguments: Value },
    Character { action: CharacterAction },
    Desktop { operation: DesktopOperation },
}
impl ServiceRequest {
    pub fn service(&self) -> Service {
        match self {
            Self::Log { .. } => Service::Log,
            Self::Configuration => Service::Configuration,
            Self::StateRead { .. } | Self::StateWrite { .. } => Service::State,
            Self::HttpOpen { .. } | Self::HttpRead { .. } | Self::HttpClose { .. } => Service::Http,
            Self::Models { .. } => Service::Models,
            Self::Tool { .. } => Service::Tools,
            Self::Character { .. } => Service::CharacterActions,
            Self::Desktop { .. } => Service::Desktop,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopOperation {
    ClipboardRead,
    ClipboardWrite {
        text: String,
    },
    Notify {
        title: String,
        body: String,
    },
    SelectFile,
    CaptureScreen,
    Avatar {
        action: CharacterAction,
    },
    ReadResource {
        handle: String,
        offset: u64,
        length: u32,
    },
    CloseResource {
        handle: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceHandle {
    pub id: String,
    pub media_type: String,
    pub size: u64,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopCapabilities {
    pub device_id: String,
    pub generation: u64,
    pub services: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
impl Failure {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            message: code.into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Initialize {
        protocol_version: u32,
        call: Call,
        entrypoint: String,
    },
    Invoke {
        call: Call,
        operation: Operation,
    },
    ServiceResult {
        call: Call,
        service_id: String,
        result: Result<Value, Failure>,
    },
    Cancel {
        call: Call,
    },
    Shutdown,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerMessage {
    Ready {
        protocol_version: u32,
        call: Call,
        capabilities: Value,
    },
    Event {
        call: Call,
        sequence: u64,
        event: Event,
    },
    Service {
        call: Call,
        service_id: String,
        request: ServiceRequest,
    },
    Complete {
        call: Call,
        result: Value,
    },
    Failed {
        call: Call,
        error: Failure,
    },
}

pub fn schemas() -> Value {
    serde_json::json!({
        "manifest": schemars::schema_for!(Manifest), "operation": schemars::schema_for!(Operation),
        "event": schemars::schema_for!(Event), "service_request": schemars::schema_for!(ServiceRequest),
        "host_message": schemars::schema_for!(HostMessage), "worker_message": schemars::schema_for!(WorkerMessage),
        "http_chunk":schemars::schema_for!(HttpChunk), "resource_handle":schemars::schema_for!(ResourceHandle),
        "desktop_capabilities":schemars::schema_for!(DesktopCapabilities), "llm_capabilities":schemars::schema_for!(LlmCapabilities)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_os_dispatch_and_caller_identity_are_rejected() {
        assert!(
            serde_json::from_value::<ServiceRequest>(
                serde_json::json!({"type":"execute_os_api","function":"system"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ServiceRequest>(
                serde_json::json!({"type":"state_read","key":"a","companion_id":"other"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<ServiceRequest>(serde_json::json!({"type":"desktop","device_id":"other","operation":{"type":"clipboard_read"}})).is_err());
        assert!(schemas()["manifest"]["properties"]["package_version"].is_object());
    }
}
