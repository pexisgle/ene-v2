//! Handing a queued reaction to the head and telling the client about it.
//!
//! The decision of *which* reaction is due lives with [`Session`] in the parent
//! module; this file owns the delivery itself: the author the head sees, the
//! diagnostic the client receives, and the bookkeeping that keeps a repeated
//! report from waking the head twice.
use super::{AUTHOR, Delivered, Reaction, ReportKind, Session};
use crate::host::{agent, notify::Notifier};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::time::Instant;

/// Runs `with` against the world's notifier. Callers that already hold other
/// world state cannot borrow it in place, so it is temporarily removed.
pub(super) fn with_notifier<R>(
    world: &mut World,
    with: impl FnOnce(&mut Notifier) -> R,
) -> Option<R> {
    let mut notifier = world.remove_resource::<Notifier>()?;
    let result = with(&mut notifier);
    world.insert_resource(notifier);
    Some(result)
}

pub(super) fn diagnostic(notifier: &mut Notifier, companion: &str, action: &str, detail: &Value) {
    notifier.diagnostic(companion, "eyes", action, detail);
}

pub(super) fn deliver(
    world: &mut World,
    entity: Entity,
    companion: &str,
    session: &mut Session,
    reaction: Reaction,
    now: Instant,
) {
    let id = RequestId(uuid::Uuid::new_v4().to_string());
    let author = Author {
        request: Some(id.clone()),
        ..Author::plugin(AUTHOR)
    };
    if agent::deliver(
        world,
        entity,
        companion,
        "[目からの通知]".into(),
        DeliveryMode::Queue,
        author,
    ) {
        with_notifier(world, |notifier| {
            diagnostic(
                notifier,
                companion,
                "delivered",
                &json!({
                    "watch": reaction.kind == ReportKind::Watch,
                    "important": reaction.kind.important(),
                    "heartbeat": reaction.kind == ReportKind::Heartbeat,
                    "report_id": id.0,
                    "summary": reaction.summary
                }),
            );
        });
        session.last_delivery = now;
        match reaction.kind {
            ReportKind::Automatic => session.last_automatic = Some((now, reaction.summary.clone())),
            ReportKind::Important | ReportKind::ResponseCheck => {
                session.last_important = Some(reaction.summary.clone())
            }
            ReportKind::Watch | ReportKind::Heartbeat => {}
        }
        session.delivered = Some(Delivered {
            id,
            summary: reaction.summary,
            kind: reaction.kind,
        });
    }
}

pub(super) fn publish_status(
    world: &mut World,
    companion: &str,
    session: &mut Session,
    status: &'static str,
) {
    if session.status == status {
        return;
    }
    session.status = status;
    world.resource_mut::<Notifier>().push(
        Some(companion),
        Some(&session.client),
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "perception_status".into(),
            payload: Some(w::JsonDocument {
                schema: "ene.perception_status.v1".into(),
                json: json!({"subscription_id": session.subscription, "status": status})
                    .to_string(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })),
    );
}
