//! Plugin manifests, typed extension APIs and the Extism guest SDK.
//! Native execution and application authorization live outside this crate.
mod api;
mod failure;
mod manifest;
pub mod provider;

pub use api::{
    AudioFormat, CharacterAction, CharacterEvent, CharacterTarget, Content, ContentBlock,
    ContentChunk, EmbeddingCapabilities, EmbeddingRequest, EmbeddingResult, Event, HarnessEvent,
    HarnessInput, HookDecision, HookEvent, HttpChunk, HttpRequest, LlmCapabilities, MessageId,
    Operation, PermissionOption, PermissionOptionId, PermissionOptionKind, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, PluginToolDefinition, RequestId, RequestPermissionOutcome,
    SelectedPermissionOutcome, ServiceRequest, SessionUpdate, SpeechCapabilities, StopReason,
    SynthesisRequest, SynthesisResult, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolInvokeRequest,
    ToolInvokeResult, ToolKind, TranscriptionRequest, TranscriptionResult, text_block,
};
pub use failure::Failure;
pub use manifest::{
    CONTRACT_VERSION, EntryPoint, Extension, Manifest, ProviderConfiguration, Service, Target,
};
pub use provider::{Model, ModelMetadata};
#[cfg(target_family = "wasm")]
mod sdk;
#[cfg(target_family = "wasm")]
pub use sdk::*;
