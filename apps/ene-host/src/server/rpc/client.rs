//! Client registration, device streams and perception.
use super::{Services, parse_client_id};
use crate::host::{client, error::HostError, resources::DeviceHub};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_domain::{ClientId, SubscriptionId};
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
        let client_id = parse_client_id(request.id)?;
        let runtime = self.runtime.clone();
        let (guard, mut receiver) = self
            .runtime
            .call_kernel(move |kernel| {
                let services = runtime.services();
                if services
                    .store
                    .client_repo()
                    .client(client_id.as_str())?
                    .is_none()
                {
                    return Err(HostError::NotFound("client not found".into()));
                }
                let hub = services.hub.clone();
                let (subscription, receiver) = hub.register(&client_id);
                let guard = StreamGuard {
                    hub,
                    client_id,
                    subscription,
                };
                kernel.client_reconnected(&guard.client_id)?;
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

/// Closes a stream only if it still owns the client. The hub wakes the kernel
/// to settle outstanding dispatches.
struct StreamGuard {
    hub: DeviceHub,
    client_id: ClientId,
    subscription: SubscriptionId,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.hub.unregister(&self.client_id, self.subscription);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Runtime;
    use crate::host::client::{DeviceOperation, DeviceStatus};
    use crate::host::db::Store;

    #[tokio::test]
    async fn a_device_stream_requires_a_valid_registered_client() {
        use connectrpc::{ErrorCode, StreamMessage};
        use w::ClientService;

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        };
        for (id, code) in [
            ("bad id", ErrorCode::InvalidArgument),
            ("missing", ErrorCode::NotFound),
        ] {
            let rpc = StreamMessage::from_message(&w::Id {
                id: id.into(),
                ..Default::default()
            });
            let result = service
                .device_stream(
                    RequestContext::new(http::HeaderMap::new()),
                    ServiceRequest::from_parts(rpc.view(), rpc.bytes()),
                )
                .await;
            assert_eq!(result.err().unwrap().code, code);
            assert!(!runtime.services().hub.connected(id));
        }
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn device_guard_cleanup_outside_tokio_preserves_replacement_and_pending_operations() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                let store = app.world().resource::<Store>();
                for (id, client, status) in [
                    ("sent", "c1", DeviceStatus::Dispatched),
                    ("unsent", "c1", DeviceStatus::Pending),
                    ("other-client", "c2", DeviceStatus::Dispatched),
                ] {
                    seed_operation(store, id, client, status)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let hub = runtime.services().hub.clone();
        let client_id = ClientId::parse("c1").unwrap();
        let (old, _old_receiver) = hub.register(&client_id);
        let old = StreamGuard {
            hub: hub.clone(),
            client_id: client_id.clone(),
            subscription: old,
        };
        let (subscription, mut receiver) = hub.register(&client_id);
        let current = StreamGuard {
            hub: hub.clone(),
            client_id,
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
        let (owner, permit) = hub.reserve("c1").unwrap();
        assert_eq!(owner, subscription);
        let _sender = permit.send(w::DeviceEvent::default());
        assert!(receiver.try_recv().is_ok());

        // Drop the current stream and install its replacement in one kernel
        // job, before the queued disconnect can run.
        let replacement = runtime
            .read(move |_| {
                let hub = current.hub.clone();
                let client_id = current.client_id.clone();
                drop(current);
                let (subscription, _receiver) = hub.register(&client_id);
                Ok(StreamGuard {
                    hub,
                    client_id,
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
        let statuses = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let store = &runtime.services().store;
            loop {
                let repo = store.client_repo();
                let sent = repo.operation("sent").unwrap().unwrap().status;
                if sent == DeviceStatus::Uncertain {
                    break (
                        sent,
                        repo.operation("unsent").unwrap().unwrap().status,
                        repo.operation("other-client").unwrap().unwrap().status,
                    );
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            statuses,
            (
                DeviceStatus::Uncertain,
                DeviceStatus::Pending,
                DeviceStatus::Dispatched
            )
        );
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn replacing_the_device_stream_settles_unanswered_dispatches() {
        use connectrpc::StreamMessage;
        use w::ClientService;

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let store = &runtime.services().store;
        store
            .client_repo()
            .register_client(&ene_store::clients::ClientDeclaration {
                id: "desk",
                name: "desk",
                platform: "test",
                capabilities: &[],
                specs: &[],
                expected_revision: 0,
            })
            .unwrap();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        };
        let rpc = StreamMessage::from_message(&w::Id {
            id: "desk".into(),
            ..Default::default()
        });
        let open = || {
            service.device_stream(
                RequestContext::new(http::HeaderMap::new()),
                ServiceRequest::from_parts(rpc.view(), rpc.bytes()),
            )
        };

        let first = open().await.unwrap();
        seed_operation(store, "sent", "desk", DeviceStatus::Dispatched).unwrap();
        let replacement = open().await.unwrap();
        let saved = store.client_repo().operation("sent").unwrap().unwrap();
        assert_eq!(saved.status, DeviceStatus::Uncertain);
        let owner = runtime.services().hub.subscription("desk").unwrap();
        drop(first);
        assert_eq!(runtime.services().hub.subscription("desk"), Some(owner));
        drop(replacement);
        runtime.shutdown().await.unwrap();
    }

    fn seed_operation(
        store: &Store,
        id: &str,
        client: &str,
        status: DeviceStatus,
    ) -> crate::host::error::Result<()> {
        use ene_domain::{CompanionId, DeviceScope};
        store.client_repo().insert_operation(&DeviceOperation {
            id: id.into(),
            client_id: client.into(),
            capability: "device.files".into(),
            input: serde_json::json!({}),
            scope: DeviceScope::Companion(CompanionId::parse("ene")?),
            status,
            output: None,
            failure: None,
            created_at: String::new(),
            updated_at: String::new(),
        })?;
        Ok(())
    }
}
