//! Client registration, device streams and perception.
use super::Services;
use crate::host::{Runtime, client, db::Store, error::HostError, resources::DeviceHub};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;

#[expect(
    refining_impl_trait,
    reason = "the Connect service trait returns boxed futures while each impl names its concrete future"
)]
impl w::ClientService for Services {
    request!(
        register_client,
        w::RegisterClientRequest,
        w::Client,
        client::register
    );

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "device_stream",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            client_id = %request.id,
        )
    )]
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
                    .client_repo()
                    .client(&client_id)?
                    .ok_or_else(|| HostError::NotFound(("client not found").into()))?;
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
        let client_id = self.client_id.clone();
        drop(self.runtime.call(move |app| {
            client::device::disconnected(app.world_mut(), &client_id);
            Ok(())
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::client::{DeviceOperation, DeviceStatus};

    #[tokio::test]
    async fn device_guard_cleanup_outside_tokio_preserves_replacement_and_pending_operations() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                let store = app.world().resource::<Store>();
                for (id, status) in [
                    ("sent", DeviceStatus::Dispatched),
                    ("unsent", DeviceStatus::Pending),
                ] {
                    store.client_repo().insert_operation(&DeviceOperation {
                        id: id.into(),
                        client_id: "c1".into(),
                        capability: "device.files".into(),
                        input: serde_json::json!({}),
                        work_id: None,
                        status,
                        output: None,
                        failure: None,
                        created_at: String::new(),
                        updated_at: String::new(),
                    })?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let hub = runtime.hub();
        let (old, _old_receiver) = hub.register("c1");
        let old = StreamGuard {
            runtime: runtime.clone(),
            hub: hub.clone(),
            client_id: "c1".into(),
            subscription: old,
        };
        let (subscription, mut receiver) = hub.register("c1");
        let current = StreamGuard {
            runtime: runtime.clone(),
            hub: hub.clone(),
            client_id: "c1".into(),
            subscription: subscription.clone(),
        };
        std::thread::spawn(move || drop(old)).join().unwrap();
        let status = runtime
            .read(|app| {
                Ok(app
                    .world()
                    .resource::<Store>()
                    .client_repo()
                    .operation("sent")?
                    .unwrap()
                    .status)
            })
            .await
            .unwrap();
        assert_eq!(hub.subscription("c1"), Some(subscription));
        assert_eq!(status, DeviceStatus::Dispatched);
        assert!(hub.send("c1", w::DeviceEvent::default()));
        assert!(receiver.try_recv().is_ok());

        std::thread::spawn(move || drop(current)).join().unwrap();
        assert!(!hub.connected("c1"));
        let statuses = runtime
            .read(|app| {
                let store = app.world().resource::<Store>();
                Ok((
                    store.client_repo().operation("sent")?.unwrap().status,
                    store.client_repo().operation("unsent")?.unwrap().status,
                ))
            })
            .await
            .unwrap();
        assert_eq!(statuses, (DeviceStatus::Uncertain, DeviceStatus::Pending));
        runtime.shutdown().await.unwrap();
    }
}
