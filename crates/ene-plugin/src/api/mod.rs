//! Extension operations, events and host services.
//!
//! Payloads live in capability modules; the shared wire enums are in [`wire`].
mod character;
mod harness;
mod http;
mod llm;
mod wire;

pub use character::{CharacterAction, CharacterEvent, CharacterTarget};
pub use harness::{
    Content, ContentBlock, ContentChunk, HarnessEvent, HarnessInput, MessageId, PermissionOption,
    PermissionOptionId, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestId, RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, text_block,
};
pub use http::{HttpChunk, HttpRequest};
pub use llm::{GenerationRequest, LlmCapabilities, ModelResponse, ToolDefinition};
pub use wire::{Event, Operation, ServiceRequest};
