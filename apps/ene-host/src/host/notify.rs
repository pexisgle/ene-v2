//! The single notification outbox and its broadcast stream.
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde_json::json;
use tokio::sync::broadcast;
use uuid::Uuid;

pub(crate) const EVENTS: usize = 256;

#[derive(Resource)]
pub(crate) struct Notifier {
    epoch: String,
    sequence: u64,
    outbox: Vec<w::Notification>,
    events: broadcast::Sender<w::Notification>,
}

impl Notifier {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Uuid::new_v4().to_string(),
            sequence: 0,
            outbox: Vec::new(),
            events: broadcast::channel(EVENTS).0,
        }
    }

    pub(crate) fn cursor(&self) -> w::Cursor {
        w::Cursor {
            epoch: self.epoch.clone(),
            seq: self.sequence,
            ..Default::default()
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<w::Notification> {
        self.events.subscribe()
    }

    pub(crate) fn push(
        &mut self,
        companion: Option<String>,
        client: Option<String>,
        event: w::notification::Event,
    ) {
        self.outbox.push(w::Notification {
            companion_id: companion,
            client_id: client,
            kind: kind_of(&event).into(),
            event: Some(event),
            ..Default::default()
        });
    }

    pub(crate) fn notice(&mut self, companion: &str, code: &str, message: &str) {
        self.push(
            Some(companion.into()),
            None,
            w::notification::Event::Generic(Box::new(w::HostEvent {
                kind: "notice".into(),
                payload: json!({
                    "code": code,
                    "message": crate::truncate_chars(message, 500),
                })
                .to_string(),
                ..Default::default()
            })),
        );
    }

    pub(crate) fn flush(&mut self) {
        let outbox = std::mem::take(&mut self.outbox);
        for mut event in outbox {
            self.sequence += 1;
            event.cursor = Some(w::Cursor {
                epoch: self.epoch.clone(),
                seq: self.sequence,
                ..Default::default()
            })
            .into();
            let _ = self.events.send(event);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.outbox.clear();
    }

    pub(crate) fn fail(&mut self, reason: &str) {
        self.outbox.clear();
        self.sequence += 1;
        let event = w::Notification {
            cursor: Some(w::Cursor {
                epoch: self.epoch.clone(),
                seq: self.sequence,
                ..Default::default()
            })
            .into(),
            kind: "host_storage_failed".into(),
            event: Some(w::notification::Event::Generic(Box::new(w::HostEvent {
                kind: "host_storage_failed".into(),
                payload: json!({
                    "code": "storage_error",
                    "message": crate::truncate_chars(reason, 1024),
                })
                .to_string(),
                ..Default::default()
            }))),
            ..Default::default()
        };
        let _ = self.events.send(event);
    }
}

fn kind_of(event: &w::notification::Event) -> &'static str {
    match event {
        w::notification::Event::Health(_) => "health",
        w::notification::Event::Companion(_) => "companion",
        w::notification::Event::MindUpdated(_) => "mind",
        w::notification::Event::MessageCreated(_) => "message_created",
        w::notification::Event::MessageDelta(_) => "message_delta",
        w::notification::Event::TurnEvent(_) => "turn",
        w::notification::Event::WorkEvent(_) => "work",
        w::notification::Event::ApprovalRequested(_) => "approval",
        w::notification::Event::PolicyRulesUpdated(_) => "policy",
        w::notification::Event::SettingsUpdated(_) => "settings",
        w::notification::Event::ClientUpdated(_) => "client",
        w::notification::Event::DeviceOperation(_) => "device_operation",
        w::notification::Event::DeviceOperationResult(_) => "device_result",
        w::notification::Event::Generic(_) => "generic",
        w::notification::Event::InitialState(_) => "initial_state",
        w::notification::Event::ResyncRequired(_) => "resync",
    }
}
