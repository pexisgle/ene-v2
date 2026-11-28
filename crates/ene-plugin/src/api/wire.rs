//! Shared wire enums for operations, events and service requests.
use super::{
    CharacterAction, CharacterEvent, CharacterTarget, EmbeddingRequest, HarnessEvent, HarnessInput,
    HookEvent, HttpChunk, HttpRequest, SynthesisRequest, ToolInvokeRequest, TranscriptionRequest,
};
use crate::manifest::{Extension, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request from the host to the plugin.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Return the models this provider serves.
    ListModels {
        /// Provider endpoint from the connection; the host overwrites it.
        endpoint: String,
        /// Adapter options from the connection; the host overwrites them.
        options: Value,
    },
    /// Forward one OpenAI-compatible exchange to the provider through the
    /// broker. The guest is a transport, not a protocol decoder.
    GenerateHttp {
        /// The HTTP exchange to perform.
        request: HttpRequest,
    },
    /// Embed the given texts.
    Embed {
        /// The texts and the model to embed them with.
        request: EmbeddingRequest,
    },
    /// Render the given text as speech.
    Synthesize {
        /// The text, voice and format to render.
        request: SynthesisRequest,
    },
    /// Turn the given audio into text.
    Transcribe {
        /// The audio and the model to recognise it with.
        request: TranscriptionRequest,
    },
    /// Tell the character plugin something happened.
    Character {
        /// The character the event is addressed to.
        target: CharacterTarget,
        /// What happened.
        event: CharacterEvent,
    },
    /// Interpret the current foreground window in a client-side sensor.
    Observe {
        /// Application identifier from the native window system.
        application: String,
        /// Foreground window title, supplied to the local guest only.
        title: String,
        /// Whether the operating system last reported a person at the desktop.
        idle: Option<bool>,
    },
    /// Advance the task harness for one turn.
    Harness {
        /// The harness input for this step.
        input: HarnessInput,
    },
    /// Report a lifecycle moment to a hook plugin.
    Hook {
        /// The lifecycle event.
        event: HookEvent,
    },
    /// Return the tools this plugin offers.
    ListTools,
    /// Run one of the tools this plugin offers.
    InvokeTool {
        /// The tool call to run.
        request: ToolInvokeRequest,
    },
}

impl Operation {
    /// The extension this operation serves.
    pub fn extension(&self) -> Extension {
        match self {
            Self::ListModels { .. } | Self::GenerateHttp { .. } => Extension::Llm,
            Self::Embed { .. } => Extension::Embedding,
            Self::Synthesize { .. } => Extension::Tts,
            Self::Transcribe { .. } => Extension::Stt,
            Self::Character { .. } => Extension::Character,
            Self::Observe { .. } => Extension::Perception,
            Self::Harness { .. } => Extension::Harness,
            Self::Hook { .. } => Extension::Hook,
            Self::ListTools | Self::InvokeTool { .. } => Extension::Tool,
        }
    }
}

/// A result or progress report from the plugin.
// Built once per plugin event and consumed immediately; boxing the harness
// variant would add an allocation to every update.
#[expect(
    clippy::large_enum_variant,
    reason = "an event is built and consumed immediately, so boxing would cost an allocation per update"
)]
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// The models this provider serves.
    Models {
        /// The model list.
        models: Vec<crate::provider::Model>,
    },
    /// One brokered HTTP chunk, verbatim. The first carries status and
    /// headers; the last has `done`.
    Http {
        /// The chunk of the response.
        chunk: HttpChunk,
    },
    /// An action for the character to perform.
    Character {
        /// The character the action is addressed to.
        target: CharacterTarget,
        /// What the character should do.
        action: CharacterAction,
    },
    /// A step of the harness turn.
    Harness {
        /// What the harness produced.
        event: HarnessEvent,
    },
    /// Streamed PCM16 audio chunk emitted by a TTS plugin (`F22`).
    Audio {
        /// The chunk's samples, base64 encoded.
        data_base64: String,
        /// Sample rate in Hz.
        #[serde(default)]
        sample_rate: u32,
        /// Whether this is the last chunk.
        #[serde(default)]
        done: bool,
    },
    /// Partial or final transcript event emitted by an STT plugin (`F22`).
    Transcription {
        /// The recognised text so far.
        text: String,
        /// Whether the provider considers this the final transcript.
        #[serde(default)]
        is_final: bool,
    },
}

impl Event {
    /// The extension this event comes from.
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

/// A plugin request for a host service.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    /// Write a line to the host log.
    Log {
        /// The line, at most 4096 bytes.
        message: String,
    },
    /// Read the plugin's configuration.
    Configuration,
    /// Read a value the plugin stored earlier.
    StateRead {
        /// Key to read; a missing key reads as null.
        key: String,
    },
    /// Store a value under a key.
    StateWrite {
        /// Key to write.
        key: String,
        /// Value to store.
        value: Value,
    },
    /// Start an HTTP exchange through the broker.
    HttpOpen {
        /// The exchange to perform.
        request: HttpRequest,
    },
    /// Read the next chunk of a brokered response.
    HttpRead {
        /// Handle the response was opened with.
        handle: String,
    },
    /// Release a brokered response.
    HttpClose {
        /// Handle to release.
        handle: String,
    },
    /// The next chunk of a synthesis answer, read back like an HTTP reply.
    AudioRead {
        /// Handle the synthesis answer was opened with.
        handle: String,
    },
    /// Call one of the host's own tools.
    Tool {
        /// The tool to call.
        name: String,
        /// Arguments for the tool.
        arguments: Value,
    },
}

impl ServiceRequest {
    /// The service this request goes to.
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
