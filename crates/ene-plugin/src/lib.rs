//! Plugin manifests, typed extension APIs and the Extism guest SDK.
//! Native execution and application authorization live outside this crate.
mod api;
mod failure;
mod manifest;
pub mod provider;

pub use api::{
    CharacterAction, CharacterEvent, CharacterTarget, Content, ContentBlock, ContentChunk, Event,
    GenerationRequest, HarnessEvent, HarnessInput, HttpChunk, HttpRequest, LlmCapabilities,
    MessageId, Operation, PermissionOption, PermissionOptionId, PermissionOptionKind, Plan,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, RequestId, RequestPermissionOutcome,
    SelectedPermissionOutcome, ServiceRequest, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, text_block,
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
