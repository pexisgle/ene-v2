use super::{auth, rpc::failure, state::App};
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use ene_rpc::rpc as w;
pub(super) struct DeviceRpc {
    pub app: App,
}
#[allow(refining_impl_trait)]
impl w::DeviceService for DeviceRpc {
    async fn register_device(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::RegisterDeviceRequest>,
    ) -> ServiceResult<w::DeviceRegistration> {
        Response::ok(self.app.devices.register(req.to_owned_message()).await?)
    }
    async fn list(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Devices> {
        Response::ok(self.app.devices.list().await?)
    }
    async fn approve(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Device> {
        Response::ok(self.app.devices.approve(req.id.into()).await?)
    }
    async fn revoke(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        self.app.devices.revoke(req.id.into()).await?;
        Response::ok(w::Empty::default())
    }
    async fn set_permission(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::DevicePermissionRequest>,
    ) -> ServiceResult<w::Empty> {
        self.app
            .devices
            .permission(
                req.device_id.into(),
                Some(req.capability.into()),
                req.allowed,
            )
            .await?;
        Response::ok(w::Empty::default())
    }
    async fn subscribe(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        auth::own_device(&ctx, req.id)?;
        let mut subscription = self.app.devices.subscribe(req.id.into()).await?;
        let principal = ctx
            .extensions()
            .get::<auth::Principal>()
            .cloned()
            .ok_or_else(|| connectrpc::ConnectError::unauthenticated("認証が必要です"))?;
        let cancelled = auth::cancelled(principal);
        let mut shutdown = self.app.application.shutdown();
        Response::stream_ok(async_stream::stream! {
        tokio::pin!(cancelled);
                    yield Ok(w::DeviceEvent{event:Some(w::device_event::Event::Ready(Box::new(w::DeviceSubscription{subscription_id:subscription.id,..Default::default()}))),..Default::default()});
                    loop {if *shutdown.borrow(){break;}tokio::select! {biased;
                        _=&mut cancelled=>break,
                        _=shutdown.changed()=>break,
                        _=subscription.changed.cancelled()=>break,
                        command=subscription.queue.recv()=>match command {
                            Some(command)=>yield Ok(w::DeviceEvent{event:Some(w::device_event::Event::Operation(Box::new(command))),..Default::default()}),None=>break,
                        }
                    }}
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
        ctx: RequestContext,
        req: ServiceRequest<'_, w::DeviceOperationResult>,
    ) -> ServiceResult<w::DeviceOperationState> {
        auth::own_device(&ctx, req.device_id)?;
        Response::ok(self.app.devices.complete(req.to_owned_message()).await?)
    }
    async fn get_operation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, w::DeviceOperationRequest>,
    ) -> ServiceResult<w::DeviceOperationState> {
        auth::own_device(&ctx, req.device_id)?;
        Response::ok(
            self.app
                .devices
                .operation(req.device_id, req.operation_id)
                .await?,
        )
    }
    async fn activate(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, w::ActivationRequest>,
    ) -> ServiceResult<w::Activation> {
        auth::own_device(&ctx, req.device_id)?;
        if req.release
            && !matches!(
                ctx.extensions().get::<auth::Principal>(),
                Some(auth::Principal::Device(_, _))
            )
        {
            return Err(connectrpc::ConnectError::permission_denied(
                "表示を止めた端末だけが表示権を返却できます",
            ));
        }
        self.app
            .application
            .companion(req.companion_id)
            .await
            .map_err(failure)?;
        Response::ok(self.app.devices.activate(req.to_owned_message()).await?)
    }
}
