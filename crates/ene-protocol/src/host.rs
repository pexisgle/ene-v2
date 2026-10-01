//! Host API wire types and notification synchronization, shared by host and client.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Companion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Streaming,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub companion_id: String,
    pub role: Role,
    pub text: String,
    pub status: MessageStatus,
    pub created_at: String,
    pub sequence: u64,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub in_reply_to: Option<String>,
    pub task_id: Option<String>,
    pub question_id: Option<String>,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Companion {
    pub id: String,
    pub name: String,
    pub body: Option<Value>,
    pub voice: Option<Value>,
    pub mind: String,
    pub model_assignments: ModelAssignments,
    pub availability: Value,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelAssignments {
    pub companion: Option<Value>,
    pub task: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub host_id: String,
    pub environment_id: String,
    pub capabilities: Value,
    pub cursor: Cursor,
    pub companions: Vec<Companion>,
    pub messages: Vec<Message>,
    pub message_cursors: BTreeMap<String, Option<String>>,
    pub tasks: Vec<Value>,
    pub questions: Vec<Value>,
    pub settings: Settings,
    pub grants: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub epoch: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub companion_id: Option<String>,
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncReason {
    EpochChanged,
    SequenceGap,
    OffsetMismatch,
    InvalidPayload,
}

/// Start only after subscribing and obtaining a snapshot for that subscription.
/// Buffer notifications during the snapshot request and pass them here in order.
pub struct Replica {
    pub cursor: Cursor,
    pub messages: BTreeMap<String, Message>,
}

impl Replica {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            cursor: snapshot.cursor.clone(),
            messages: snapshot
                .messages
                .iter()
                .map(|m| (m.id.clone(), m.clone()))
                .collect(),
        }
    }

    /// Returns false for notifications already covered by the snapshot.
    /// A rejected event never advances the cursor or changes a message.
    pub fn apply(&mut self, event: &Notification) -> Result<bool, ResyncReason> {
        if event.epoch != self.cursor.epoch {
            return Err(ResyncReason::EpochChanged);
        }
        if event.seq <= self.cursor.seq {
            return Ok(false);
        }
        if self.cursor.seq.checked_add(1) != Some(event.seq) {
            return Err(ResyncReason::SequenceGap);
        }
        match event.kind.as_str() {
            "message.created" | "message.started" | "message.updated" => {
                let message: Message = serde_json::from_value(event.data.clone())
                    .map_err(|_| ResyncReason::InvalidPayload)?;
                if event.companion_id.as_deref() != Some(&message.companion_id) {
                    return Err(ResyncReason::InvalidPayload);
                }
                self.messages.insert(message.id.clone(), message);
            }
            "message.delta" => {
                #[derive(Deserialize)]
                struct Delta {
                    message_id: String,
                    offset: usize,
                    delta: String,
                }
                let delta: Delta = serde_json::from_value(event.data.clone())
                    .map_err(|_| ResyncReason::InvalidPayload)?;
                let message = self
                    .messages
                    .get_mut(&delta.message_id)
                    .ok_or(ResyncReason::OffsetMismatch)?;
                if event.companion_id.as_deref() != Some(&message.companion_id)
                    || message.status != MessageStatus::Streaming
                    || message.text.chars().count() != delta.offset
                {
                    return Err(ResyncReason::OffsetMismatch);
                }
                message.text.push_str(&delta.delta);
            }
            // Forward-compatible notifications still consume a sequence number.
            _ => {}
        }
        self.cursor.seq = event.seq;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replica() -> Replica {
        let m = Message {
            id: "m".into(),
            companion_id: "c".into(),
            role: Role::Companion,
            text: "日本🙂e\u{301}".into(),
            status: MessageStatus::Streaming,
            created_at: "2026-10-01T00:00:00Z".into(),
            sequence: 2,
            resources: vec![],
            in_reply_to: Some("u".into()),
            task_id: None,
            question_id: None,
            failure: None,
        };
        Replica {
            cursor: Cursor {
                epoch: "e".into(),
                seq: 8,
            },
            messages: [(m.id.clone(), m)].into(),
        }
    }

    fn delta(seq: u64, offset: usize) -> Notification {
        Notification {
            epoch: "e".into(),
            seq,
            kind: "message.delta".into(),
            companion_id: Some("c".into()),
            data: json!({"message_id":"m", "offset":offset, "delta":"。"}),
        }
    }

    #[test]
    fn unicode_scalar_offsets_and_snapshot_overlap() {
        let mut r = replica();
        assert_eq!(r.apply(&delta(8, 0)), Ok(false));
        assert_eq!(r.apply(&delta(9, 5)), Ok(true));
        assert_eq!(r.messages["m"].text, "日本🙂e\u{301}。");
        assert_eq!(r.apply(&delta(9, 5)), Ok(false));
    }

    #[test]
    fn broken_stream_leaves_state_unchanged() {
        for (event, reason) in [
            (delta(10, 5), ResyncReason::SequenceGap),
            (delta(9, 11), ResyncReason::OffsetMismatch),
            (
                Notification {
                    epoch: "new".into(),
                    ..delta(9, 5)
                },
                ResyncReason::EpochChanged,
            ),
            (
                Notification {
                    data: json!({}),
                    ..delta(9, 5)
                },
                ResyncReason::InvalidPayload,
            ),
        ] {
            let mut r = replica();
            let before = r.messages.clone();
            assert_eq!(r.apply(&event), Err(reason));
            assert_eq!(r.cursor.seq, 8);
            assert_eq!(r.messages, before);
        }
    }

    #[test]
    fn unknown_event_advances_and_final_update_replaces_partial_text() {
        let mut r = replica();
        let mut event = delta(9, 0);
        event.kind = "future.event".into();
        assert_eq!(r.apply(&event), Ok(true));
        let mut m = r.messages["m"].clone();
        m.status = MessageStatus::Completed;
        m.text = "完了".into();
        event.seq = 10;
        event.kind = "message.updated".into();
        event.data = serde_json::to_value(&m).unwrap();
        assert_eq!(r.apply(&event), Ok(true));
        assert_eq!(r.messages["m"], m);
    }
}
