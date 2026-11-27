//! Handing a queued reaction to the head and telling the client about it.
//!
//! The decision of *which* reaction is due lives with [`Session`] in the parent
//! module; this file owns the delivery itself: the author the head sees, the
//! diagnostic the client receives, and the bookkeeping that keeps a repeated
//! report from waking the head twice.
use super::{AUTHOR, Delivered, Reaction, ReportKind, Session};
use crate::host::{
    agent,
    notify::{Notifier, generic},
};
use bevy_ecs::prelude::*;
use rig_core::conversation::{Author, RequestId};
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::time::Instant;

pub(super) fn diagnostic(notifier: &Notifier, companion: &str, action: &str, detail: &Value) {
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
        if let Some(notifier) = world.get_resource::<Notifier>() {
            diagnostic(
                notifier,
                companion,
                "delivered",
                &json!({
                    "watch": reaction.kind == ReportKind::Watch,
                    "important": reaction.kind.important(),
                    "heartbeat": reaction.kind == ReportKind::Heartbeat,
                    "report_id": id.0,
                }),
            );
        }
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
    notifier: &Notifier,
    companion: &str,
    session: &mut Session,
    status: &'static str,
) {
    if session.status == status {
        return;
    }
    session.status = status;
    notifier.push(
        Some(companion),
        Some(&session.client),
        generic(
            "perception_status",
            "ene.perception_status.v1",
            &json!({"subscription_id": session.subscription, "status": status}),
        ),
    );
}
