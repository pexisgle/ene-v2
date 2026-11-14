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

    read_id!(get_companion, w::Companion, mind::get);
    request!(
        create_companion,
        w::CreateCompanionRequest,
        w::Companion,
        mind::create
    );
    request!(
        update_companion,
        w::UpdateCompanionRequest,
        w::Companion,
        mind::update
    );
    discard_id!(delete_companion, w::Id, id, mind::delete);
    read_id!(get_mind, w::MindView, mind::mind_view);
    request!(
        update_mind_profile,
        w::UpdateMindProfileRequest,
        w::MindView,
        mind::update_profile
    );
    request!(
        control_memory,
        w::MemoryControlRequest,
        w::MemoryControlResult,
        mind::control_memory
    );
    request!(read list_memories, w::ListMemoriesRequest, w::Memories, mind::list_memories);
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
