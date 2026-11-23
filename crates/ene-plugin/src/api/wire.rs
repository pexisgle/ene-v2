//! Shared wire enums for operations, events and service requests.
use super::{
    CharacterAction, CharacterEvent, CharacterTarget, EmbeddingRequest, HarnessEvent, HarnessInput,
    HookEvent, HttpChunk, HttpRequest, SynthesisRequest, ToolInvokeRequest, TranscriptionRequest,
};
use crate::manifest::{Extension, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ListModels {
        endpoint: String,
        options: Value,
    },
    /// Forward one OpenAI-compatible exchange to the provider through the
    /// broker. The guest is a transport, not a protocol decoder.
    GenerateHttp {
        request: HttpRequest,
    },
    Embed {
        request: EmbeddingRequest,
    },
    Synthesize {
        request: SynthesisRequest,
    },
    Transcribe {
        request: TranscriptionRequest,
    },
    Character {
        target: CharacterTarget,
        event: CharacterEvent,
    },
    Harness {
        input: HarnessInput,
    },
    Hook {
        event: HookEvent,
    },
    ListTools,
    InvokeTool {
        request: ToolInvokeRequest,
    },
}

impl Operation {
    pub fn extension(&self) -> Extension {
        match self {
            Self::ListModels { .. } | Self::GenerateHttp { .. } => Extension::Llm,
            Self::Embed { .. } => Extension::Embedding,
            Self::Synthesize { .. } => Extension::Tts,
            Self::Transcribe { .. } => Extension::Stt,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
            Self::Hook { .. } => Extension::Hook,
            Self::ListTools | Self::InvokeTool { .. } => Extension::Tool,
        }
    }
}

// Built once per plugin event and consumed immediately; boxing the harness
// variant would add an allocation to every update.
#[expect(
    clippy::large_enum_variant,
    reason = "an event is built and consumed immediately, so boxing would cost an allocation per update"
)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Models {
        models: Vec<crate::provider::Model>,
    },
    /// One brokered HTTP chunk, verbatim. The first carries status and
    /// headers; the last has `done`.
    Http {
        chunk: HttpChunk,
    },
    Character {
        target: CharacterTarget,
        action: CharacterAction,
    },
    Harness {
        event: HarnessEvent,
    },
    /// Streamed PCM16 audio chunk emitted by a TTS plugin (`F22`).
    Audio {
        data_base64: String,
        #[serde(default)]
        sample_rate: u32,
        #[serde(default)]
        done: bool,
    },
    /// Partial or final transcript event emitted by an STT plugin (`F22`).
    Transcription {
        text: String,
        #[serde(default)]
        is_final: bool,
    },
}

impl Event {
    pub fn extension(&self) -> Extension {
        match self {
            Self::Models { .. } | Self::Http { .. } => Extension::Llm,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
            Self::Audio { .. } => Extension::Tts,
            Self::Transcription { .. } => Extension::Stt,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Log {
        message: String,
    },
    Configuration,
    StateRead {
        key: String,
    },
    StateWrite {
        key: String,
        value: Value,
    },
    HttpOpen {
        request: HttpRequest,
    },
    HttpRead {
        handle: String,
    },
    HttpClose {
        handle: String,
    },
    /// The next chunk of a synthesis answer, read back like an HTTP reply.
    AudioRead {
        handle: String,
    },
    Tool {
        name: String,
        arguments: Value,
    },
}

impl ServiceRequest {
    pub fn service(&self) -> Service {
        match self {
            Self::Log { .. } => Service::Log,
            Self::Configuration => Service::Configuration,
            Self::StateRead { .. } | Self::StateWrite { .. } => Service::State,
            Self::HttpOpen { .. } | Self::HttpRead { .. } | Self::HttpClose { .. } => Service::Http,
            Self::AudioRead { .. } => Service::Http,
            Self::Tool { .. } => Service::Tools,
        }
    }
}
