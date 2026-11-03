//! The API's view of the kernel's conversation.
//!
//! The conversation is Rig's; these functions only read it. A message id is a
//! conversation entry id, a sequence is the entry's position and a stream is
//! the request entry the turn answers.

use crate::domain::{Error, ErrorKind};
use ene_rpc as w;
use rig_core::completion::Message;
use rig_core::conversation::{EntryRef, MessageId, TurnEnd};
use rig_ecs::prelude::*;

/// How a turn that has no end yet is shown.
const OPEN: &str = "incomplete";

/// One terminal failure in the wire shape, with the message bounded.
pub(super) fn failure(code: &str, message: &str) -> w::Failure {
    w::Failure {
        code: code.into(),
        message: message.chars().take(1024).collect(),
        details: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

struct TurnSpan {
    start: usize,
    request_entry: Option<String>,
    request_id: Option<String>,
    end: Option<TurnEnd>,
}

impl TurnSpan {
    fn status(&self) -> &'static str {
        match &self.end {
            None => OPEN,
            Some(TurnEnd::Answered { .. }) => "completed",
            Some(TurnEnd::Failed { .. }) => "failed",
            Some(TurnEnd::Stopped { .. }) => "cancelled",
        }
    }

    fn failure(&self) -> Option<w::Failure> {
        let (code, message) = match &self.end {
            Some(TurnEnd::Failed { why }) => (
                if why == "output_limit" {
                    "output_limit"
                } else {
                    "generation_failed"
                },
                why.as_str(),
            ),
            Some(TurnEnd::Stopped { why, .. }) => (
                "cancelled",
                why.as_deref().unwrap_or("停止操作により生成を停止しました"),
            ),
            _ => return None,
        };
        Some(failure(code, message))
    }
}

fn spans(conversation: &Conversation) -> Vec<TurnSpan> {
    let mut spans = conversation
        .turns()
        .map(|(_, record)| {
            let request = conversation.request(record.start);
            TurnSpan {
                start: conversation.start_of(record.start),
                request_entry: request.as_ref().map(|entry| entry.id.to_string()),
                request_id: request
                    .as_ref()
                    .and_then(|entry| entry.author)
                    .and_then(|author| author.request.as_ref())
                    .map(|request| request.0.clone()),
                end: record.end,
            }
        })
        .collect::<Vec<_>>();
    spans.sort_unstable_by_key(|span| span.start);
    spans
}

/// The turn `index` belongs to: the last one that began at or before it.
fn span_of(spans: &[TurnSpan], index: usize) -> Option<&TurnSpan> {
    spans.iter().rev().find(|span| span.start <= index)
}

fn user_text(message: &Message) -> Option<String> {
    let Message::User { content } = message else {
        return None;
    };
    let text = content
        .iter()
        .filter_map(|item| match item {
            rig_core::message::UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then(|| text.trim().to_owned())
}

fn assistant_text(message: &Message) -> Option<String> {
    let Message::Assistant(reply) = message else {
        return None;
    };
    let text = reply.text();
    (!text.trim().is_empty()).then(|| text.trim().to_owned())
}

/// Whether an entry is a message the client shows: the user's own text or a
/// model reply with text. Tool results and tool-call-only replies are kept by
/// Rig but are not conversation messages.
fn visible(entry: &EntryRef<'_>) -> bool {
    entry.is_users_own() || assistant_text(entry.message).is_some()
}

fn created_at(made: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(made) * 1_000_000)
        .ok()
        .and_then(|time| {
            time.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn entry_message(spans: &[TurnSpan], entry: &EntryRef<'_>, companion: &str) -> Option<w::Message> {
    let span = span_of(spans, entry.index);
    let (role, text) = if entry.is_users_own() {
        ("user", user_text(entry.message)?)
    } else {
        ("assistant", assistant_text(entry.message)?)
    };
    let failure = (role == "assistant")
        .then(|| span.and_then(TurnSpan::failure))
        .flatten();
    Some(w::Message {
        id: entry.id.to_string(),
        companion_id: companion.to_owned(),
        role: role.into(),
        text,
        // A user's message was recorded when it was accepted; the turn's
        // state is about the reply, not about their text.
        status: if role == "user" {
            "completed"
        } else {
            span.map(TurnSpan::status).unwrap_or(OPEN)
        }
        .into(),
        created_at: created_at(entry.made),
        sequence: entry.index as u64 + 1,
        resources: Vec::new(),
        in_reply_to: (role == "assistant")
            .then(|| span.and_then(|span| span.request_entry.clone()))
            .flatten(),
        task_id: None,
        question_id: None,
        failure: failure.into(),
        request_id: if role == "assistant" {
            span.and_then(|span| span.request_id.clone())
        } else {
            entry
                .author
                .and_then(|author| author.request.as_ref())
                .map(|request| request.0.clone())
        },
        ..Default::default()
    })
}

/// Every message the client shows, oldest first.
pub(super) fn visible_messages(conversation: &Conversation, companion: &str) -> Vec<w::Message> {
    let spans = spans(conversation);
    conversation
        .entries()
        .filter(|entry| visible(entry))
        .filter_map(|entry| entry_message(&spans, &entry, companion))
        .collect()
}

fn parse_cursor(companion: &str, cursor: &str) -> Result<u64, Error> {
    let (sequence, id) = cursor.split_once(':').ok_or_else(Error::invalid)?;
    if id != companion {
        return Err(Error::invalid());
    }
    let sequence: u64 = sequence.parse().map_err(|_| Error::invalid())?;
    if sequence == 0 {
        return Err(Error::invalid());
    }
    Ok(sequence)
}

pub(super) fn history(
    conversation: &Conversation,
    companion: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<w::History, Error> {
    let before = cursor
        .map(|cursor| parse_cursor(companion, cursor))
        .transpose()?;
    let all = visible_messages(conversation, companion);
    let end = before
        .map(|before| {
            all.iter()
                .position(|message| message.sequence >= before)
                .unwrap_or(all.len())
        })
        .unwrap_or(all.len());
    let start = end.saturating_sub(limit);
    let items = all[start..end].to_vec();
    let next_cursor = (start > 0)
        .then(|| {
            items
                .first()
                .map(|message| format!("{}:{companion}", message.sequence))
        })
        .flatten();
    Ok(w::History {
        items,
        next_cursor,
        ..Default::default()
    })
}

pub(super) fn message(
    conversation: &Conversation,
    companion: &str,
    id: &str,
) -> Result<w::Message, Error> {
    let spans = spans(conversation);
    conversation
        .entries()
        .find(|entry| entry.id.to_string() == id && visible(entry))
        .and_then(|entry| entry_message(&spans, &entry, companion))
        .ok_or_else(|| Error::new(ErrorKind::NotFound))
}

/// The id of the reply entry a turn ended with, when the conversation holds
/// one after `start`.
pub(super) fn reply_entry(conversation: &Conversation, start: MessageId) -> Option<String> {
    let start = conversation.start_of(start);
    conversation
        .entries()
        .rfind(|entry| entry.index >= start && assistant_text(entry.message).is_some())
        .map(|entry| entry.id.to_string())
}

/// Whether the conversation or its saved inbox carries this request id. The
/// id is the host's stable reference for an accepted input.
pub(super) fn has_request(conversation: &Conversation, request_id: &str) -> bool {
    let in_entries = conversation.entries().any(|entry| {
        entry
            .author
            .and_then(|author| author.request.as_ref())
            .is_some_and(|request| request.0 == request_id)
    });
    if in_entries {
        return true;
    }
    conversation.asides().any(|(_, aside)| {
        aside.kind == "rig.inbox"
            && serde_json::from_value::<rig_ecs::inbox::Incoming>(aside.value.clone())
                .ok()
                .and_then(|message| message.author.request)
                .is_some_and(|request| request.0 == request_id)
    })
}

/// The user entry that asked for the turn, when it is still in the
/// conversation.
pub(super) fn request_entry(conversation: &Conversation, start: MessageId) -> Option<EntryRef<'_>> {
    conversation.request(start)
}
