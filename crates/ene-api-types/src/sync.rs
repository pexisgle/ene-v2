//! Pure notification application and resynchronization rules.
use crate::host::*;
use serde::Deserialize;
use std::collections::BTreeMap;
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterOptions {
    pub only: Vec<String>,
    pub allow_fallbacks: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: OpenRouterOptions,
    pub credential_state: String,
    pub capabilities: Option<Value>,
    pub last_check: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataTransferGrant {
    pub id: String,
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
    pub status: String,
    pub revision: u64,
    pub invalidation_reason: Option<String>,
    pub connection_fingerprint: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MessageAcceptance {
    pub request_id: String,
    pub message_id: String,
    pub response_message_id: String,
}
