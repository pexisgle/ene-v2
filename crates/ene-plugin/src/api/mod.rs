//! Extension operations, events and host services.
//!
//! Payloads live in capability modules; the shared wire enums are in [`wire`].
mod audio;
mod character;
mod embedding;
mod harness;
mod hook;
mod http;
mod llm;
mod tool;
mod wire;

pub use audio::{
    AudioFormat, SpeechCapabilities, SynthesisRequest, SynthesisResult, TranscriptionRequest,
    TranscriptionResult,
};
pub use character::{CharacterAction, CharacterEvent, CharacterTarget};
pub use embedding::{EmbeddingCapabilities, EmbeddingRequest, EmbeddingResult};
pub use harness::{
    Content, ContentBlock, ContentChunk, HarnessEvent, HarnessInput, MessageId, PermissionOption,
    PermissionOptionId, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestId, RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, text_block,
};
pub use hook::{HookDecision, HookEvent};
pub use http::{HttpChunk, HttpRequest, MAX_HTTP_HEADER_BYTES, MAX_HTTP_HEADER_FIELDS};
pub use llm::LlmCapabilities;
pub use tool::{PluginToolDefinition, ToolInvokeRequest, ToolInvokeResult};
pub use wire::{Event, Operation, ServiceRequest};
