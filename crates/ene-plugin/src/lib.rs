//! Plugin manifests, typed extension APIs and the Extism guest SDK.
//! Native execution and application authorization live outside this crate.
mod api;
mod failure;
mod manifest;
pub mod provider;
mod window;

pub use api::{
    AudioChunk, CharacterAction, CharacterEvent, CharacterTarget, ContentBlock, ContentChunk,
    DesktopCapabilities, DesktopOperation, Event, GenerationRequest, HarnessEvent, HarnessInput,
    HttpChunk, HttpRequest, LlmCapabilities, MessageId, ModelResponse, Operation, PermissionOption,
    PermissionOptionId, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestId, RequestPermissionOutcome, ResourceHandle, ServiceRequest, SessionUpdate,
    SpeechInput, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolDefinition, ToolKind,
};
pub use failure::Failure;
pub use manifest::{
    Asset, CONTRACT_VERSION, EntryPoint, Extension, Manifest, ProviderConfiguration, Service,
    Target,
};
pub use provider::{Model, ModelMetadata};
#[cfg(target_family = "wasm")]
mod sdk;
#[cfg(target_family = "wasm")]
pub use sdk::*;
pub use window::{
    WindowBounds, WindowCapabilities, WindowEvent, WindowEventKind, WindowId, WindowInfo,
    WindowSubscription,
};
