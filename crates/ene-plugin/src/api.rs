//! Extension operations, events and host services.
use crate::manifest::{Extension, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmCapabilities {
    pub streaming_text: bool,
    pub model_list: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub cancellation: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterAction {
    Speak { text: String },
    Expression { name: String, duration_ms: u32 },
    Motion { asset: String, duration_ms: u32 },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessInput {
    Start { instruction: String },
    Answer { question_id: String, answer: String },
    Result { operation_id: String, result: Value },
    Cancel,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_base64: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpChunk {
    pub handle: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub data_base64: String,
    pub done: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceHandle {
    pub id: String,
    pub media_type: String,
    pub size: u64,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopCapabilities {
    pub device_id: String,
    pub generation: u64,
    pub services: Vec<String>,
}
