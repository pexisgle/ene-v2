//! Host snapshot, subscription and companion/mind services.
use super::Services;
use crate::host::{self, db::Store, mind};
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;

#[allow(refining_impl_trait)]
impl w::HostService for Services {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Snapshot> {
        self.read(host::snapshot).await
    }

    async fn subscribe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeRequest>,
    ) -> ServiceResult<ServiceStream<w::Notification>> {
        let client_id = (!request.client_id.is_empty()).then(|| request.client_id.to_owned());
        let (snapshot, mut events) = self
            .runtime
            .read(move |app| host::subscribe(app, client_id.as_deref()))
            .await
            .map_err(ConnectError::from)?;
        let initial = w::Notification {
            cursor: snapshot.cursor.clone(),
            kind: "initial_state".into(),
            event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
            ..Default::default()
        };
        let target_cursor = snapshot_cursor(&initial);
        let stream = async_stream::stream! {
            yield Ok(initial);
            loop {
                match events.recv().await {
                    Ok(event) => yield Ok(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(w::Notification {
                            kind: "resync".into(),
                            event: Some(w::notification::Event::ResyncRequired(Box::new(
                                w::ResyncRequired {
                                    reason: "subscription lagged".into(),
                                    target_cursor: target_cursor.clone().into(),
                                    ..Default::default()
                                },
                            ))),
                            ..Default::default()
                        });
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn list_companions(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Companions> {
        self.read(|app| {
            Ok(w::Companions {
                items: mind::list(app.world().resource::<Store>())?,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Companion> {
        let id = request.id.to_owned();
        self.read(move |app| mind::get(app.world().resource::<Store>(), &id))
            .await
    }

    async fn create_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CreateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |app| mind::create(app.world_mut(), request))
            .await
    }

    async fn update_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateCompanionRequest>,
    ) -> ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |app| mind::update(app.world_mut(), request))
            .await
    }

    async fn delete_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        self.call(move |app| {
            mind::delete(app.world_mut(), &id)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn get_mind(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::MindView> {
        let id = request.id.to_owned();
        self.read(move |app| mind::mind_view(app.world().resource::<Store>(), &id))
            .await
    }

    async fn update_mind_profile(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateMindProfileRequest>,
    ) -> ServiceResult<w::MindView> {
        let request = request.to_owned_message();
        self.call(move |app| mind::update_profile(app.world_mut(), request))
            .await
    }

    async fn control_memory(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::MemoryControlRequest>,
    ) -> ServiceResult<w::MemoryControlResult> {
        let request = request.to_owned_message();
        self.call(move |app| mind::control_memory(app.world_mut(), request))
            .await
    }

    async fn list_memories(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListMemoriesRequest>,
    ) -> ServiceResult<w::Memories> {
        let request = request.to_owned_message();
        self.read(move |app| mind::list_memories(app.world_mut(), request))
            .await
    }
}

fn snapshot_cursor(notification: &w::Notification) -> w::Cursor {
    match notification.event.as_ref() {
        Some(w::notification::Event::InitialState(snapshot)) => {
            snapshot.cursor.as_option().cloned().unwrap_or_default()
        }
        _ => w::Cursor::default(),
    }
}
