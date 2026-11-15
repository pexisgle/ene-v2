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
    let output = extism_pdk::ManagedMemory::new(unsafe { ene_service(input.offset()) }.into());
    let Json(result) = output.as_ref().to::<Json<Result<T, Failure>>>()?;
    Ok(result?)
}
pub fn emit(event: Event) -> Result<(), extism_pdk::Error> {
    unsafe { ene_emit(Json(event)) }
}
pub fn http_open(request: HttpRequest) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::HttpOpen { request })
}
pub fn http_read(handle: String) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::HttpRead { handle })
}
pub fn http_close(handle: String) -> Result<(), extism_pdk::Error> {
    service(ServiceRequest::HttpClose { handle })
}
/// The next chunk of a synthesis answer, read back like an HTTP reply.
pub fn audio_read(handle: String) -> Result<HttpChunk, extism_pdk::Error> {
    service(ServiceRequest::AudioRead { handle })
}
