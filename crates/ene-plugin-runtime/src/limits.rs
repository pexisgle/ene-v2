//! Bounds for embedded WebAssembly and its application-defined host functions.
use std::time::Duration;

/// Operation and service requests, including a 512 KiB HTTP body encoded as Base64.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Individual events, service responses and return values.
pub const MAX_CHUNK: usize = 128 * 1024;
/// Maximum events retained by one non-streaming host invocation.
pub const MAX_EVENTS: usize = 256;
/// Streaming operations can produce more events under bounded channel backpressure.
pub const MAX_STREAM_EVENTS: u64 = 65_536;
pub const MAX_SERVICE_CALLS: usize = 4096;
/// Aggregate request/response budget for application services in one invocation.
pub const MAX_SERVICE_BYTES: usize = 16 * 1024 * 1024;
/// Aggregate event budget for one invocation, including provider streams.
pub const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_INSTANCES: usize = 16;
/// Persistent guests hold instance slots; leave room for provider generations.
pub const MAX_SESSIONS: usize = MAX_INSTANCES / 2;
pub const MAX_CODE: u64 = 32 * 1024 * 1024;
pub const MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const WASM_MEMORY_PAGES: u32 = 1024;
pub const WASM_TABLE_ELEMENTS: usize = 20_000;
pub const WASM_FUEL: u64 = 100_000_000;
pub const WASM_INITIALIZATION_FUEL: u64 = 10_000_000;
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
pub const EXECUTION_TIMEOUT: Duration = Duration::from_secs(180);
/// A harness turn runs inside one instance call and only yields on client
/// requests, so it needs a wall-clock budget for a whole task segment.
pub const HARNESS_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
