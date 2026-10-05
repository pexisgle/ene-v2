//! The event kind, owner and payload are derived together; wire strings stay here.
use super::{Boundary, HostApplication, Notification};
use crate::domain::*;
use serde_json::json;

pub(super) enum Change {
    Companion(Companion),
    Settings(Settings),
    Grants(Vec<Grant>),
    MessageCreated(Message),
    MessageStarted(Message),
    MessageUpdated(Message),
    MessageDelta { message: Message, delta: String },
    PersistenceFailed { companion: String, message: String },
    RequestPersistenceFailed { companion: String, request: String },
}

pub(super) fn publish(boundary: &mut Boundary, app: &HostApplication, change: Change) {
    let (kind, companion_id, data) = match change {
        Change::Companion(c) => ("companion.updated", Some(c.id.clone()), json!(c)),
        Change::Settings(s) => ("settings.updated", None, json!(s)),
        Change::Grants(g) => ("grants.updated", None, json!(g)),
        Change::MessageCreated(m) => ("message.created", Some(m.companion_id.clone()), json!(m)),
        Change::MessageStarted(m) => ("message.started", Some(m.companion_id.clone()), json!(m)),
        Change::MessageUpdated(m) => ("message.updated", Some(m.companion_id.clone()), json!(m)),
        Change::MessageDelta { message, delta } => (
            "message.delta",
            Some(message.companion_id.clone()),
            json!({"message_id":message.id,"offset":message.text.chars().count()-delta.chars().count(),"delta":delta}),
        ),
        Change::PersistenceFailed { companion, message } => (
            "generation.persistence_failed",
            Some(companion),
            json!({"message_id":message,"code":"storage_error"}),
        ),
        Change::RequestPersistenceFailed { companion, request } => (
            "generation.persistence_failed",
            Some(companion),
            json!({"request_id":request,"code":"storage_error"}),
        ),
    };
    boundary.cursor.seq += 1;
    let _ = app.0.events.send(Notification {
        epoch: boundary.cursor.epoch.clone(),
        seq: boundary.cursor.seq,
        kind: kind.into(),
        companion_id,
        data,
    });
}
