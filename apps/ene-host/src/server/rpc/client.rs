//! Client registration, device streams and perception.
use super::Services;
use crate::host::{Runtime, client, error::HostError, resources::DeviceHub};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_domain::SubscriptionId;
use ene_rpc as w;
use futures_util::StreamExt;

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::ClientService for Services {
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "register_client", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn register_client(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::RegisterClientRequest>,
    ) -> connectrpc::ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.register_client(&request))
            .await
    }

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
        self.inspect(|services| {
            Ok(w::Clients {
                items: client::clients(&services.store, &services.hub)?,
                ..Default::default()
            })
        })
        .await
    }

    #[tracing::instrument(name = "rpc", skip_all, fields(method = "activate", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn activate(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::ActivationRequest>,
    ) -> connectrpc::ServiceResult<w::Activation> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.activate_client(&request))
            .await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "set_capabilities", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn set_capabilities(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::SetCapabilitiesRequest>,
    ) -> connectrpc::ServiceResult<w::Client> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.set_capabilities(&request))
            .await
    }

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
        self.with_store({
            let client_id = client_id.clone();
            move |store| {
                store
                    .client_repo()
                    .client(&client_id)?
                    .ok_or_else(|| HostError::NotFound(("client not found").into()))?;
                Ok(())
            }
        })
        .await?;

        let runtime = self.runtime.clone();
        let (guard, mut receiver) = self
            .runtime
            .call_kernel(move |kernel| {
                let hub = runtime.hub();
                let (subscription, receiver) = hub.register(&client_id);
                let guard = StreamGuard {
                    runtime,
                    hub,
                    client_id,
                    subscription,
                };
                kernel.client_reconnected(&guard.client_id);
                Ok((guard, receiver))
            })
            .await
            .map_err(ConnectError::from)?;
        let stream = async_stream::stream! {
            yield Ok(w::DeviceEvent {
                event: Some(w::device_event::Event::Ready(Box::new(w::DeviceReady {
                    subscription_id: guard.subscription.to_string(),
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

    #[tracing::instrument(name = "rpc", skip_all, fields(method = "report_perception", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn report_perception(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::PerceptionEvent>,
    ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
        let request = request.to_owned_message();
        self.call(move |kernel| {
            kernel.report_perception(&request)?;
            Ok(ene_rpc::Empty::default())
        })
        .await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "complete_operation", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn complete_operation(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::DeviceOperationResult>,
    ) -> connectrpc::ServiceResult<w::DeviceOperationState> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.complete_operation(&request))
            .await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "get_operation", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn get_operation(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::Id>,
    ) -> connectrpc::ServiceResult<w::DeviceOperationState> {
        let id = request.id.to_owned();
        self.read(move |kernel| kernel.operation_state(&id)).await
    }
}

/// Closes a device stream when the response stream is dropped, including a
/// client disconnect. Operations that can no longer be answered become
/// uncertain; a stream superseded by a newer one leaves them alone.
struct StreamGuard {
    runtime: Runtime,
    hub: DeviceHub,
    client_id: String,
    subscription: SubscriptionId,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if !self.hub.unregister(&self.client_id, self.subscription) {
            return;
        }
        let client_id = self.client_id.clone();
        drop(self.runtime.call_kernel(move |kernel| {
            kernel.client_disconnected(&client_id);
            Ok(())
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::client::{DeviceOperation, DeviceStatus};
    use crate::host::db::Store;

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
            subscription,
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

        // Drop the current stream and install its replacement in one kernel
        // job, before the queued disconnect can run.
        let runtime_for_guard = runtime.clone();
        let replacement = runtime
            .read(move |_| {
                let hub = current.hub.clone();
                drop(current);
                let (subscription, _receiver) = hub.register("c1");
                Ok(StreamGuard {
                    runtime: runtime_for_guard,
                    hub,
                    client_id: "c1".into(),
                    subscription,
                })
            })
            .await
            .unwrap();
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
        assert_eq!(status, DeviceStatus::Dispatched);
        std::thread::spawn(move || drop(replacement))
            .join()
            .unwrap();
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
