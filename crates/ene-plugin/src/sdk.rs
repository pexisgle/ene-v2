//! Extism owns the guest ABI; these wrappers expose ene's typed service contract.
use crate::{Event, Failure, HttpChunk, HttpRequest, ServiceRequest};
pub use extism_pdk::{self, FnResult, Json, plugin_fn};
use serde::de::DeserializeOwned;

#[link(wasm_import_module = "extism:host/user")]
unsafe extern "C" {
    fn ene_service(input: u64) -> u64;
}

#[extism_pdk::host_fn("extism:host/user")]
extern "ExtismHost" {
    fn ene_emit(event: Json<Event>);
}

/// Decode the response directly into its expected type, preserving service failures.
///
/// PDK 1.4.1's host_fn macro does not free host-returned memory, so this wrapper
/// keeps the response in ManagedMemory until decoding finishes.
pub fn service<T: DeserializeOwned>(request: ServiceRequest) -> Result<T, extism_pdk::Error> {
    let input = extism_pdk::ManagedMemory::from(extism_pdk::ToMemory::to_memory(&Json(request))?);
    // SAFETY: `ene_service` is the Extism host function declared above; it takes the
    // offset of memory the host owns and returns an offset it allocated, which
    // `ManagedMemory::new` adopts and frees. `input` outlives the call, so the
    // offset stays valid for its whole duration.
    let output = extism_pdk::ManagedMemory::new(unsafe { ene_service(input.offset()) }.into());
    let Json(result) = output.as_ref().to::<Json<Result<T, Failure>>>()?;
    Ok(result?)
}
/// Reports an event to the host without expecting a reply.
pub fn emit(event: Event) -> Result<(), extism_pdk::Error> {
    // SAFETY: `ene_emit` is the host function declared above. It reads the JSON it
    // is handed and copies it before returning, so `event` need not outlive the call.
    unsafe { ene_emit(Json(event)) }
}
/// Starts an HTTP exchange through the host broker.
pub fn http_open(request: HttpRequest) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::HttpOpen { request })
}
/// Reads the next chunk of a brokered response.
pub fn http_read(handle: String) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::HttpRead { handle })
}
/// Releases a brokered response.
pub fn http_close(handle: String) -> Result<(), extism_pdk::Error> {
    service(ServiceRequest::HttpClose { handle })
}
/// The next chunk of a synthesis answer, read back like an HTTP reply.
pub fn audio_read(handle: String) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::AudioRead { handle })
}
/// Emits a structured log message to the host (`Service::Log`).
pub fn log(message: impl Into<String>) -> Result<(), extism_pdk::Error> {
    let _: serde_json::Value = service(ServiceRequest::Log {
        message: message.into(),
    })?;
    Ok(())
}
/// Reads this plugin's saved configuration document (`Service::Configuration`).
pub fn configuration() -> Result<serde_json::Value, extism_pdk::Error> {
    service(ServiceRequest::Configuration)
}
/// Reads a value from this plugin's isolated key-value state (`Service::State`).
pub fn state_read(key: impl Into<String>) -> Result<Option<serde_json::Value>, extism_pdk::Error> {
    service(ServiceRequest::StateRead { key: key.into() })
}
/// Writes a value into this plugin's isolated key-value state (`Service::State`).
pub fn state_write(
    key: impl Into<String>,
    value: serde_json::Value,
) -> Result<(), extism_pdk::Error> {
    let _: serde_json::Value = service(ServiceRequest::StateWrite {
        key: key.into(),
        value,
    })?;
    Ok(())
}
/// Calls an authorized host tool (`Service::Tools`).
pub fn call_tool(
    name: impl Into<String>,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, extism_pdk::Error> {
    service(ServiceRequest::Tool {
        name: name.into(),
        arguments,
    })
}
