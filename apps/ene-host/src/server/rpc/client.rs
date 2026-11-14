//! Client registration, device streams and perception.
use super::Services;
use crate::host::{Runtime, client, db::Store, error::HostError, resources::DeviceHub};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;

#[allow(refining_impl_trait)]
impl w::ClientService for Services {
    request!(
        register_client,
        w::RegisterClientRequest,
        w::Client,
        client::register
    );

    async fn list(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Clients> {
        self.read(|app| {
            let store = app.world().resource::<Store>().clone();
            let hub = app.world().resource::<DeviceHub>().clone();
            Ok(w::Clients {
                items: client::clients(&store, &hub)?,
                ..Default::default()
            })
        })
        .await
    }

    request!(
        activate,
        w::ActivationRequest,
        w::Activation,
        client::activate
    );
    request!(
        set_capabilities,
        w::SetCapabilitiesRequest,
        w::Client,
        client::set_capabilities
    );

    async fn device_stream(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<ServiceStream<w::DeviceEvent>> {
        let client_id = request.id.to_owned();
        self.read({
            let client_id = client_id.clone();
            move |app| {
                let store = app.world().resource::<Store>().clone();
                store
                    .client(&client_id)?
                    .ok_or_else(|| HostError::not_found("client not found"))?;
                Ok(())
            }
        })
        .await?;

        let hub = self.runtime.hub();
        let (subscription, mut receiver) = hub.register(&client_id);
        // Replay operations the client has not received yet.
        if let Err(error) = self
            .runtime
            .call({
                let client_id = client_id.clone();
                move |app| {
                    client::device::resend(app.world_mut(), &client_id);
                    Ok(())
                }
            })
            .await
        {
            hub.unregister(&client_id, &subscription);
            return Err(ConnectError::from(error));
        }

        let guard = StreamGuard {
            runtime: self.runtime.clone(),
            hub,
            client_id: client_id.clone(),
            subscription: subscription.clone(),
        };
        let stream = async_stream::stream! {
            yield Ok(w::DeviceEvent {
                event: Some(w::device_event::Event::Ready(Box::new(w::DeviceReady {
                    subscription_id: subscription,
                    ..Default::default()
                }))),
                ..Default::default()
            });
            while let Some(event) = receiver.recv().await {
                yield Ok(event);
            }
            drop(guard);
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    discard!(
        report_perception,
        w::PerceptionEvent,
        client::report_perception
    );
    request!(
        complete_operation,
        w::DeviceOperationResult,
        w::DeviceOperationState,
        client::device::complete
    );
    request!(read get_operation, w::Id, w::DeviceOperationState, client::device::get);
}

/// Closes a device stream when the response stream is dropped, including a
/// client disconnect. Operations that can no longer be answered become
/// uncertain; a stream superseded by a newer one leaves them alone.
struct StreamGuard {
    runtime: Runtime,
    hub: DeviceHub,
    client_id: String,
    subscription: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let current =
            self.hub.subscription(&self.client_id).as_deref() == Some(self.subscription.as_str());
        self.hub.unregister(&self.client_id, &self.subscription);
        if !current {
            return;
        }
        let runtime = self.runtime.clone();
        let client_id = self.client_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime
                    .call(move |app| {
                        client::device::disconnected(app.world_mut(), &client_id);
                        Ok(())
                    })
                    .await;
            });
        }
    }
}
