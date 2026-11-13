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
        let request = request.to_owned_message();
        let subscriber = (!request.client_id.is_empty()).then(|| request.client_id.clone());
        let subscriber_view = subscriber.clone();
        let cursor = request.cursor.as_option().cloned();
        // A cursor from this epoch resumes from the replay ring; an unknown or
        // stale cursor (or a gap) falls back to a fresh snapshot.
        let replayed = match cursor.as_ref() {
            Some(cursor) => self
                .runtime
                .read({
                    let cursor = cursor.clone();
                    move |app| {
                        Ok(app
                            .world()
                            .resource::<crate::host::notify::Notifier>()
                            .replay(&cursor))
                    }
                })
                .await
                .map_err(ConnectError::from)?,
            None => None,
        };
        let (snapshot, mut events) = self
            .runtime
            .read(move |app| host::subscribe(app, subscriber.as_deref()))
            .await
            .map_err(ConnectError::from)?;
        let target_cursor = snapshot.cursor.clone();
        let start = match replayed {
            Some(events) => Start::Replay(events),
            None => Start::Snapshot(Box::new(w::Notification {
                cursor: snapshot.cursor.clone(),
                kind: "initial_state".into(),
                event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
                ..Default::default()
            })),
        };
        let stream = async_stream::stream! {
            match start {
                Start::Replay(events) => {
                    for event in events {
                        if visible(&event, subscriber_view.as_deref()) {
                            yield Ok(event);
                        }
                    }
                }
                Start::Snapshot(initial) => yield Ok(*initial),
            }
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if visible(&event, subscriber_view.as_deref()) {
                            yield Ok(event);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(w::Notification {
                            kind: "resync".into(),
                            event: Some(w::notification::Event::ResyncRequired(Box::new(
                                w::ResyncRequired {
                                    reason: "subscription lagged".into(),
                                    target_cursor: target_cursor.clone(),
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

/// How a subscription starts: replayed events, or a snapshot when resuming is
/// not possible.
enum Start {
    Replay(Vec<w::Notification>),
    Snapshot(Box<w::Notification>),
}

/// A notification addressed to one client is not sent to another.
fn visible(event: &w::Notification, subscriber: Option<&str>) -> bool {
    match (event.client_id.as_deref(), subscriber) {
        (Some(target), Some(subscriber)) => target == subscriber,
        _ => true,
    }
}
