//! Extism owns the guest ABI; these wrappers expose ene's typed service contract.
pub use ene_ipc::plugin::*;
#[cfg(target_family = "wasm")]
pub use extism_pdk::{self, FnResult, Json, plugin_fn};
#[cfg(target_family = "wasm")]
mod guest {
    use super::*;
    use extism_pdk::*;
    use serde_json::Value;
    #[host_fn]
    extern "ExtismHost" {
        fn ene_service(request: String) -> String;
        fn ene_emit(event: String);
    }
    pub fn service(request: ServiceRequest) -> FnResult<Value> {
        let result = unsafe { ene_service(serde_json::to_string(&request)?)? };
        let result: Result<Value, Failure> = serde_json::from_str(&result)?;
        Ok(result.map_err(|e| extism_pdk::Error::msg(e.code))?)
    }
    pub fn emit(event: Event) -> FnResult<()> {
        unsafe { ene_emit(serde_json::to_string(&event)?)? };
        Ok(())
    }
    pub fn http_open(request: HttpRequest) -> FnResult<HttpChunk> {
        Ok(serde_json::from_value(service(
            ServiceRequest::HttpOpen { request },
        )?)?)
    }
    pub fn http_read(handle: String) -> FnResult<HttpChunk> {
        Ok(serde_json::from_value(service(
            ServiceRequest::HttpRead { handle },
        )?)?)
    }
    pub fn http_close(handle: String) -> FnResult<Value> {
        service(ServiceRequest::HttpClose { handle })
    }
}
#[cfg(target_family = "wasm")]
pub use guest::*;
