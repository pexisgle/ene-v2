//! Client registration, device streams and perception.
use super::Services;
use crate::host::{self, client};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;

#[allow(refining_impl_trait)]
impl w::ClientService for Services {
    async fn register_client(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::RegisterClientRequest>,
    ) -> ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |app| client::register(app, request)).await
    }

    async fn list(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Clients> {
        self.read(|app| {
            Ok(w::Clients {
                items: client::list(
                    app.world().resource::<host::db::Store>(),
                    app.world().resource::<host::resources::DeviceStreams>(),
                )?,
                ..Default::default()
            })
        })
        .await
    }

    async fn activate(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ActivationRequest>,
    ) -> ServiceResult<w::Activation> {
        let request = request.to_owned_message();
        self.call(move |app| client::activate(app, request)).await
    }

    async fn set_capabilities(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SetCapabilitiesRequest>,
    ) -> ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |app| client::set_capabilities(app, request))
            .await
    }

    async fn report_perception(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::PerceptionEvent>,
    ) -> ServiceResult<w::Empty> {
        let event = request.to_owned_message();
        self.call(move |app| {
            client::report_perception(app.world_mut(), event)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn device_stream(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        let client_id = request.id.to_owned();
        let subscription = self
            .runtime
            .call(move |app| client::subscribe(app.world_mut(), &client_id))
            .await
            .map_err(ConnectError::from)?;
        let (id, mut receiver, pending) =
            (subscription.id, subscription.receiver, subscription.pending);
        let stream = async_stream::stream! {
            yield Ok(w::DeviceEvent {
                event: Some(w::device_event::Event::Ready(Box::new(w::DeviceReady {
                    subscription_id: id,
                    ..Default::default()
                }))),
                ..Default::default()
            });
            for operation in pending {
                yield Ok(w::DeviceEvent {
                    event: Some(w::device_event::Event::Operation(Box::new(operation))),
                    ..Default::default()
                });
            }
            while let Some(operation) = receiver.recv().await {
                yield Ok(w::DeviceEvent {
                    event: Some(w::device_event::Event::Operation(Box::new(operation))),
                    ..Default::default()
                });
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn complete_operation(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::DeviceOperationResult>,
    ) -> ServiceResult<w::DeviceOperationState> {
        let request = request.to_owned_message();
        self.call(move |app| client::complete(app.world_mut(), request))
            .await
    }

    async fn get_operation(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::DeviceOperationState> {
        let id = request.id.to_owned();
        self.read(move |app| client::get_operation(app.world_mut(), &id))
            .await
    }
}
