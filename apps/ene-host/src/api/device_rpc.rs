use super::{rpc::failure, state::App};
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use ene_rpc::rpc as w;

pub(super) struct DeviceRpc {
    pub app: App,
}

#[allow(refining_impl_trait)]
impl w::DeviceService for DeviceRpc {
    async fn subscribe(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        let mut subscription = self.app.devices.subscribe(req.id.into()).await?;
        let mut shutdown = self.app.application.shutdown();
        Response::stream_ok(async_stream::stream! {
            yield Ok(w::DeviceEvent{event:Some(w::device_event::Event::Ready(Box::new(w::DeviceSubscription{subscription_id:subscription.id,..Default::default()}))),..Default::default()});
            loop {
                if *shutdown.borrow() { break; }
                tokio::select! { biased;
                    _ = shutdown.changed() => break,
                    _ = subscription.changed.cancelled() => break,
                    command = subscription.queue.recv() => match command {
                        Some(command) => yield Ok(w::DeviceEvent{event:Some(w::device_event::Event::Operation(Box::new(command))),..Default::default()}),
                        None => break,
                    }
                }
            }
        })
    }

    async fn request_operation(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::DeviceOperationRequest>,
    ) -> ServiceResult<w::DeviceOperationState> {
        Response::ok(self.app.devices.request(req.to_owned_message()).await?)
    }

    async fn complete_operation(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::DeviceOperationResult>,
    ) -> ServiceResult<w::DeviceOperationState> {
        Response::ok(self.app.devices.complete(req.to_owned_message()).await?)
    }

    async fn get_operation(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::DeviceOperationRequest>,
    ) -> ServiceResult<w::DeviceOperationState> {
        Response::ok(
            self.app
                .devices
                .operation(req.device_id, req.operation_id)
                .await?,
        )
    }

    async fn activate(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::ActivationRequest>,
    ) -> ServiceResult<w::Activation> {
        self.app
            .application
            .companion(req.companion_id)
            .await
            .map_err(failure)?;
        Response::ok(self.app.devices.activate(req.to_owned_message()).await?)
    }
}
