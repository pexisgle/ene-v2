//! The single notification outbox and its broadcast stream.
//!
//! Systems push events into the outbox; one tick assigns the epoch/sequence
//! cursor and broadcasts them. Persisted state stays canonical, so a client
//! that misses events can always resnapshot.
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde_json::json;
use std::collections::VecDeque;
use tokio::sync::broadcast;
use uuid::Uuid;

pub(crate) const CAPACITY: usize = 256;
/// How many flushed notifications are kept for a resuming cursor.
pub(crate) const REPLAY: usize = 512;

#[derive(Resource)]
pub(crate) struct Notifier {
    epoch: String,
    sequence: u64,
    outbox: Vec<w::Notification>,
    history: VecDeque<w::Notification>,
    events: broadcast::Sender<w::Notification>,
}

impl Notifier {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Uuid::new_v4().to_string(),
            sequence: 0,
            outbox: Vec::new(),
            history: VecDeque::new(),
            events: broadcast::channel(CAPACITY).0,
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

    /// Queues one event for the next tick.
    pub(crate) fn push(
        &mut self,
        companion: Option<&str>,
        client: Option<&str>,
        event: w::notification::Event,
    ) {
        self.outbox.push(w::Notification {
            kind: kind_of(&event).into(),
            companion_id: companion.map(ToOwned::to_owned),
            client_id: client.map(ToOwned::to_owned),
            event: Some(event),
            ..Default::default()
        });
    }

    /// A host-level notice the clients can display.
    pub(crate) fn notice(&mut self, companion: Option<&str>, code: &str, message: &str) {
        let document = w::JsonDocument {
            schema: "ene.notice.v1".into(),
            json: json!({"code": code, "message": message}).to_string(),
            ..Default::default()
        };
        self.push(
            companion,
            None,
            w::notification::Event::Generic(Box::new(w::HostEvent {
                kind: "notice".into(),
                payload: Some(document).into(),
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
            self.history.push_back(event.clone());
            if self.history.len() > REPLAY {
                self.history.pop_front();
            }
            let _ = self.events.send(event);
        }
    }

    /// The events after `cursor`, when the caller's epoch is this boot and no
    /// event it needs has been dropped. `None` asks the caller to resnapshot.
    pub(crate) fn replay(&self, cursor: &w::Cursor) -> Option<Vec<w::Notification>> {
        if cursor.epoch != self.epoch || cursor.seq > self.sequence {
            return None;
        }
        let first_needed = cursor.seq + 1;
        if first_needed > self.sequence {
            return Some(Vec::new());
        }
        let oldest = self.history.front().and_then(seq_of);
        if oldest.is_none_or(|oldest| oldest > first_needed) {
            return None;
        }
        Some(
            self.history
                .iter()
                .filter(|event| seq_of(event).is_some_and(|seq| seq >= first_needed))
                .cloned()
                .collect(),
        )
    }

    pub(crate) fn clear(&mut self) {
        self.outbox.clear();
    }

    /// Reports a fatal host failure to every subscriber.
    pub(crate) fn fail(&mut self, reason: &str) {
        self.outbox.clear();
        self.sequence += 1;
        let document = w::JsonDocument {
            schema: "ene.notice.v1".into(),
            json: json!({"code": "storage_error", "message": reason}).to_string(),
            ..Default::default()
        };
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
                payload: Some(document).into(),
                ..Default::default()
            }))),
            ..Default::default()
        };
        let _ = self.events.send(event);
    }
}

fn seq_of(event: &w::Notification) -> Option<u64> {
    event.cursor.as_option().map(|cursor| cursor.seq)
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
        w::notification::Event::ActivationUpdated(_) => "activation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str) -> w::notification::Event {
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: kind.into(),
            ..Default::default()
        }))
    }

    #[test]
    fn a_cursor_resumes_from_the_replay_ring_and_reports_gaps() {
        let mut notifier = Notifier::new();
        notifier.push(None, None, event("one"));
        notifier.flush();
        let after_one = notifier.cursor();
        notifier.push(None, None, event("two"));
        notifier.push(None, None, event("three"));
        notifier.flush();

        let replay = notifier.replay(&after_one).expect("same epoch");
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[1].cursor.as_option().unwrap().seq, 3);

        let current = notifier.cursor();
        assert_eq!(notifier.replay(&current).unwrap().len(), 0);

        let mut stale = current.clone();
        stale.seq = 0;
        stale.epoch = "another boot".into();
        assert!(notifier.replay(&stale).is_none());

        let mut ahead = current.clone();
        ahead.seq = 99;
        assert!(notifier.replay(&ahead).is_none());
    }
}
