//! Shared plugin manifests, extension APIs and worker communication contracts.
//! Plugin runtimes and SDKs share these contracts; guest execution and manifest
//! validation belong to the plugin runtime. Extism owns the Wasm guest ABI.
mod api;
mod manifest;
mod protocol;
pub mod provider;
mod window;

pub use api::{
    AudioChunk, CharacterAction, CharacterEvent, CharacterTarget, DesktopCapabilities,
    DesktopOperation, Event, GenerationRequest, HarnessEvent, HarnessInput, HttpChunk, HttpRequest,
    LlmCapabilities, Operation, ResourceHandle, ServiceRequest, SpeechInput, ToolDefinition,
};
pub use manifest::{
    Asset, CONTRACT_VERSION, EntryPoint, Extension, Manifest, ProviderConfiguration, Runtime,
    Service, Target,
};
pub use protocol::{Call, Failure, HostMessage, WorkerMessage};
pub use window::{
    WindowBounds, WindowCapabilities, WindowEvent, WindowEventKind, WindowId, WindowInfo,
    WindowSubscription,
};
