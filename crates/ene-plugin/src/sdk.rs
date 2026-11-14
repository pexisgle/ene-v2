//! Extism owns the guest ABI; these wrappers expose ene's typed service contract.
use crate::{Event, Failure, HttpChunk, HttpRequest, ServiceRequest};
pub use extism_pdk::{self, FnResult, Json, plugin_fn};
use serde::de::DeserializeOwned;

#[link(wasm_import_module = "extism:host/user")]
unsafe extern "C" {
    fn ene_service(input: u64) -> u64;
    fn ene_emit(input: u64);
}

/// Decode the response directly into its expected type, preserving service failures.
///
/// The PDK host_fn macro does not free host-returned memory; this wrapper owns
/// and frees that allocation explicitly.
pub fn service<T: DeserializeOwned>(request: ServiceRequest) -> Result<T, extism_pdk::Error> {
    let input = extism_pdk::ManagedMemory::from(extism_pdk::ToMemory::to_memory(&Json(request))?);
    let output = extism_pdk::Memory::from(unsafe { ene_service(input.offset()) });
    let bytes = output.to_vec();
    output.free();
    let Json(result) = <Json<Result<T, Failure>> as extism_pdk::FromBytes>::from_bytes(&bytes)?;
    Ok(result?)
}
pub fn emit(event: Event) -> Result<(), extism_pdk::Error> {
    let input = extism_pdk::ManagedMemory::from(extism_pdk::ToMemory::to_memory(&Json(event))?);
    unsafe { ene_emit(input.offset()) };
    Ok(())
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
