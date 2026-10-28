use super::*;
use rig_http::http_client::{HttpClientExt, LazyBody, MultipartForm, StreamingResponse};
use rig_http::wasm_compat::WasmCompatSend;

impl HttpClientExt for AuthorizedTransport {
    fn send<T, U>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let transport = self.clone();
        let request = request.map(Into::into);
        async move {
            let response = transport.send(request).await.map_err(rig_error)?;
            let mut builder = http::Response::builder().status(response.status());
            *builder.headers_mut().expect("response headers") = response.headers().clone();
            let body: LazyBody<U> =
                Box::pin(async move { response.bytes().await.map(U::from).map_err(rig_error) });
            Ok(builder.body(body)?)
        }
    }
    // The Rig contract requires a 'static future; async fn captures &self.
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        _: http::Request<MultipartForm>,
    ) -> impl Future<Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        async { Err(rig_error(Error::new(ErrorKind::UnsupportedCapability))) }
    }
    fn send_streaming<T>(
        &self,
        request: http::Request<T>,
    ) -> impl Future<Output = rig_http::http_client::Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let transport = self.clone();
        let request = request.map(Into::into);
        async move {
            let mut response = transport.send(request).await.map_err(rig_error)?;
            let mut builder = http::Response::builder().status(response.status());
            *builder.headers_mut().expect("response headers") = response.headers().clone();
            let body: rig_http::http_client::BoxedStream = Box::pin(async_stream::try_stream! {
                let mut framing = EventBudget::default();
                while let Some(bytes) = response.chunk().await.map_err(rig_error)? {
                    framing.accept(&bytes).map_err(rig_error)?;
                    yield bytes;
                }
            });
            Ok(builder.body(body)?)
        }
    }
}
fn rig_error(error: Error) -> rig_http::http_client::Error {
    rig_http::http_client::Error::instance(error)
}
