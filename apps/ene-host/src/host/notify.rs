//! The single notification outbox and its broadcast stream.
//!
//! Systems push events into the outbox; one tick assigns the epoch/sequence
//! cursor and broadcasts them. Persisted state stays canonical, so a client
//! that misses events can always resnapshot.
use bevy_ecs::prelude::*;
use ene_rpc as w;
use parking_lot::Mutex;
use serde_json::json;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::broadcast;
use uuid::Uuid;

const CAPACITY: usize = 256;
/// How many flushed notifications are kept for a resuming cursor.
const REPLAY: usize = 512;
pub(crate) const DIAGNOSTIC: &str = "diagnostic";

pub(crate) fn generic(
    kind: &str,
    schema: &str,
    payload: &serde_json::Value,
) -> w::notification::Event {
    w::notification::Event::Generic(Box::new(w::HostEvent {
        kind: kind.into(),
        payload: Some(w::JsonDocument {
            schema: schema.into(),
            json: payload.to_string(),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }))
}

#[derive(Clone, Resource)]
pub(crate) struct Notifier {
    state: Arc<Mutex<Outbox>>,
    events: broadcast::Sender<Arc<w::Notification>>,
}

struct Outbox {
    cursor: w::Cursor,
    pending: Vec<w::Notification>,
    history: VecDeque<Arc<w::Notification>>,
}

impl Notifier {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Outbox {
                cursor: w::Cursor {
                    epoch: Uuid::new_v4().to_string(),
                    ..Default::default()
                },
                pending: Vec::new(),
                history: VecDeque::new(),
            })),
            events: broadcast::channel(CAPACITY).0,
        }
    }

    pub(crate) fn cursor(&self) -> w::Cursor {
        self.state.lock().cursor.clone()
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<w::Notification>> {
        self.events.subscribe()
    }

    /// Queues one event for the next tick.
    pub(crate) fn push(
        &self,
        companion: Option<&str>,
        client: Option<&str>,
        event: w::notification::Event,
    ) {
        let kind = kind_of(&event);
        self.state.lock().pending.push(w::Notification {
            kind: kind.into(),
            emitted_at: if matches!(kind, "turn" | "work" | "approval") {
                ene_store::util::now()
            } else {
                String::new()
            },
            companion_id: companion.map(ToOwned::to_owned),
            client_id: client.map(ToOwned::to_owned),
            event: Some(event),
            ..Default::default()
        });
    }

    /// Adds operational metadata to opt-in subscriptions.
    pub(crate) fn diagnostic(
        &self,
        companion: &str,
        role: &str,
        action: &str,
        detail: &serde_json::Value,
    ) {
        self.state.lock().pending.push(w::Notification {
            kind: DIAGNOSTIC.into(),
            emitted_at: ene_store::util::now(),
            companion_id: Some(companion.to_owned()),
            event: Some(generic(
                DIAGNOSTIC,
                "ene.diagnostic.v1",
                &json!({"role": role, "action": action, "detail": detail}),
            )),
            ..Default::default()
        });
    }

    /// A host-level notice the clients can display.
    pub(crate) fn notice(&self, companion: Option<&str>, code: &str, message: &str) {
        self.push(
            companion,
            None,
            generic(
                "notice",
                "ene.notice.v1",
                &json!({"code": code, "message": message}),
            ),
        );
    }

    pub(crate) fn flush(&self) {
        let Outbox {
            cursor,
            pending,
            history,
        } = &mut *self.state.lock();
        for mut event in pending.drain(..) {
            cursor.seq += 1;
            event.cursor = Some(cursor.clone()).into();
            let event = Arc::new(event);
            history.push_back(Arc::clone(&event));
            if history.len() > REPLAY {
                history.pop_front();
            }
            if self.events.send(event).is_err() {
                tracing::debug!("no live notification subscriber");
            }
        }
    }

    /// The events after `cursor`, when the caller's epoch is this boot and no
    /// event it needs has been dropped. `None` asks the caller to resnapshot.
    pub(crate) fn replay(&self, cursor: &w::Cursor) -> Option<Vec<Arc<w::Notification>>> {
        let state = self.state.lock();
        if cursor.epoch != state.cursor.epoch || cursor.seq > state.cursor.seq {
            return None;
        }
        if cursor.seq == state.cursor.seq {
            return Some(Vec::new());
        }
        if seq_of(state.history.front()?)? > cursor.seq + 1 {
            return None;
        }
        let start = state
            .history
            .partition_point(|event| seq_of(event).is_some_and(|seq| seq <= cursor.seq));
        Some(state.history.range(start..).cloned().collect())
    }

    pub(crate) fn clear(&self) {
        self.state.lock().pending.clear();
    }

    /// Reports a fatal host failure to every subscriber.
    pub(crate) fn fail(&self, reason: &str) {
        let mut state = self.state.lock();
        state.pending.clear();
        state.cursor.seq += 1;
        let event = w::Notification {
            cursor: Some(state.cursor.clone()).into(),
            kind: "host_storage_failed".into(),
            event: Some(generic(
                "host_storage_failed",
                "ene.notice.v1",
                &json!({"code": "storage_error", "message": reason}),
            )),
            ..Default::default()
        };
        ene_domain::debug_if_err(
            self.events
                .send(Arc::new(event))
                .map_err(|_| "the notification stream has no receiver"),
            "notification delivery",
        );
    }
}

fn seq_of(event: &w::Notification) -> Option<u64> {
    event.cursor.as_option().map(|cursor| cursor.seq)
}

fn kind_of(event: &w::notification::Event) -> &'static str {
    match event {
        w::notification::Event::Health(_) => "health",
        w::notification::Event::Companion(_) => "companion",
        w::notification::Event::CompanionRemoved(_) => "companion_removed",
        w::notification::Event::MindUpdated(_) => "mind",
        w::notification::Event::MessageCreated(_) => "message_created",
        w::notification::Event::MessageDelta(_) => "message_delta",
        w::notification::Event::TurnEvent(_) => "turn",
        w::notification::Event::WorkEvent(_) => "work",
        w::notification::Event::ApprovalRequested(_) => "approval",
        w::notification::Event::PolicyRulesUpdated(_) => "policy",
        w::notification::Event::PolicyTextsUpdated(_) => "policy_text",
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
        let notifier = Notifier::new();
        notifier.push(None, None, event("one"));
        notifier.flush();
        let after_one = notifier.cursor();
        let mut live = notifier.subscribe();
        notifier.push(None, None, event("two"));
        notifier.push(None, None, event("three"));
        notifier.flush();

        let replay = notifier.replay(&after_one).expect("same epoch");
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[1].cursor.as_option().unwrap().seq, 3);
        assert!(Arc::ptr_eq(&replay[0], &live.try_recv().unwrap()));
        assert!(Arc::ptr_eq(&replay[1], &live.try_recv().unwrap()));

        let current = notifier.cursor();
        assert_eq!(notifier.replay(&current).unwrap().len(), 0);

        let mut stale = current.clone();
        stale.seq = 0;
        stale.epoch = "another boot".into();
        assert!(notifier.replay(&stale).is_none());

        let mut ahead = current.clone();
        ahead.seq = 99;
        assert!(notifier.replay(&ahead).is_none());

        for _ in 0..REPLAY {
            notifier.push(None, None, event("later"));
        }
        notifier.flush();
        assert!(notifier.replay(&after_one).is_none());
        let replay = notifier.replay(&current).expect("first retained event");
        assert_eq!(replay.len(), REPLAY);
        assert_eq!(replay[0].cursor.as_option().unwrap().seq, current.seq + 1);
    }
}
