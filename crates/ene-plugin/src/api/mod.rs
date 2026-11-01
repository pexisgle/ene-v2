//! Extension operations, events and host services.
//!
//! Payloads live in capability modules; the shared wire enums are in [`wire`].
mod character;
mod desktop;
mod harness;
mod http;
mod llm;
mod speech;
mod wire;

pub use character::{CharacterAction, CharacterEvent, CharacterTarget};
pub use desktop::{DesktopCapabilities, DesktopOperation, ResourceHandle};
pub use harness::{
    ContentBlock, ContentChunk, HarnessEvent, HarnessInput, MessageId, PermissionOption,
    PermissionOptionId, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestId, RequestPermissionOutcome, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
};
pub use http::{HttpChunk, HttpRequest};
pub use llm::{GenerationRequest, LlmCapabilities, ModelResponse, ToolDefinition};
pub use speech::{AudioChunk, SpeechInput};
pub use wire::{Event, Operation, ServiceRequest};
