//! Extism owns the guest ABI; these wrappers expose ene's typed service contract.
use crate::*;
#[cfg(target_family = "wasm")]
pub use extism_pdk::{self, FnResult, Json, plugin_fn};
#[cfg(target_family = "wasm")]
mod guest {
    use super::*;
    use extism_pdk::*;
    use serde::de::DeserializeOwned;
    use serde_json::Value;
    #[host_fn]
    extern "ExtismHost" {
        fn ene_service<T: DeserializeOwned>(
            request: Json<ServiceRequest>,
        ) -> Json<Result<T, Failure>>;
        fn ene_emit(event: Json<Event>);
    }
    /// Decode the host response directly into its expected type.
    fn service_result<T: DeserializeOwned>(request: ServiceRequest) -> FnResult<T> {
        let Json(result) = unsafe { ene_service(Json(request))? };
        Ok(result.map_err(|e| extism_pdk::Error::msg(format!("{}: {}", e.code, e.message)))?)
    }
    pub fn service(request: ServiceRequest) -> FnResult<Value> {
        service_result(request)
    }
    pub fn emit(event: Event) -> FnResult<()> {
        unsafe { ene_emit(Json(event))? };
        Ok(())
    }
    pub fn http_open(request: HttpRequest) -> FnResult<HttpChunk> {
        service_result(ServiceRequest::HttpOpen { request })
    }
    pub fn http_read(handle: String) -> FnResult<HttpChunk> {
        service_result(ServiceRequest::HttpRead { handle })
    }
    pub fn http_close(handle: String) -> FnResult<Value> {
        service(ServiceRequest::HttpClose { handle })
    }
}
#[cfg(target_family = "wasm")]
pub use guest::*;
