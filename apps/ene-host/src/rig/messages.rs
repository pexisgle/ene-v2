//! The API's view of the kernel's conversation.
//!
//! The conversation is Rig's; these functions only read it. A message id is a
//! conversation entry id, a sequence is the entry's position and a stream is
//! the request entry the turn answers.

use crate::domain::{Error, ErrorKind};
use ene_rpc as w;
use rig_core::completion::Message;
use rig_core::conversation::{Entry, EntryRef, MessageId, TurnEnd};
use rig_ecs::prelude::*;

/// How a turn that has no end yet is shown.
const OPEN: &str = "incomplete";

/// One terminal failure in the wire shape, with the message bounded.
pub(super) fn failure(code: &str, message: &str) -> w::Failure {
    w::Failure {
        code: code.into(),
        message: crate::truncate_chars(message, 1024),
        details: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

/// A failed turn in the wire shape: a discarded output is not a provider error.
pub(super) fn turn_failure(why: &str) -> w::Failure {
    if why == "output_limit" {
        failure("output_limit", "返答が出力上限を超えたため破棄しました")
    } else {
        failure("generation_failed", why)
    }
}

/// A stopped turn in the wire shape.
pub(super) fn stopped(reason: Option<&str>) -> w::Failure {
    failure(
        "cancelled",
        reason.unwrap_or("停止操作により生成を停止しました"),
    )
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
        match &self.end {
            Some(TurnEnd::Failed { why }) => Some(turn_failure(why)),
            Some(TurnEnd::Stopped { why, .. }) => Some(stopped(why.as_deref())),
            _ => None,
        }
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

/// An entry the client shows, with the role and text the wire shape needs.
/// Tool results and tool-call-only replies are kept by Rig but are not
/// conversation messages.
struct Shown<'a> {
    entry: EntryRef<'a>,
    role: &'static str,
    text: String,
}

/// The text of an entry the client shows: the user's own input or a model
/// reply with text. Tool results and tool-call-only replies have none.
fn shown_text(entry: &EntryRef<'_>) -> Option<String> {
    if entry.is_users_own() {
        user_text(entry.message)
    } else {
        assistant_text(entry.message)
    }
}

/// The messages the client shows, oldest first.
fn shown(conversation: &Conversation) -> impl Iterator<Item = Shown<'_>> + '_ {
    conversation.entries().filter_map(|entry| {
        let text = shown_text(&entry)?;
        let role = if entry.is_users_own() {
            "user"
        } else {
            "assistant"
        };
        Some(Shown { entry, role, text })
    })
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

fn entry_message(spans: &[TurnSpan], shown: &Shown<'_>, companion: &str) -> w::Message {
    let entry = shown.entry;
    let (role, text) = (shown.role, shown.text.clone());
    let span = span_of(spans, entry.index);
    let failure = (role == "assistant")
        .then(|| span.and_then(TurnSpan::failure))
        .flatten();
    w::Message {
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
        in_reply_to: (role == "assistant")
            .then(|| span.and_then(|span| span.request_entry.clone()))
            .flatten(),
        failure: failure.into(),
        turn_status: span.map(TurnSpan::status).unwrap_or(OPEN).into(),
        request_id: if role == "assistant" {
            span.and_then(|span| span.request_id.clone())
        } else {
            entry
                .author
                .and_then(|author| author.request.as_ref())
                .map(|request| request.0.clone())
        },
        ..Default::default()
    }
}

/// The highest message id the client has already been shown.
pub(super) fn last_shown_id(conversation: &Conversation) -> Option<MessageId> {
    shown(conversation).map(|shown| shown.entry.id).max()
}

/// Every message the client has not been shown yet, oldest first, with the
/// highest id they carry. Rig never reuses an id, so this one id is all the
/// state a notification needs.
pub(super) fn unseen_messages(
    conversation: &Conversation,
    companion: &str,
    after: Option<MessageId>,
) -> (Vec<w::Message>, Option<MessageId>) {
    let spans = spans(conversation);
    let mut cursor = after;
    let mut messages = Vec::new();
    for shown in shown(conversation) {
        if after.is_some_and(|after| shown.entry.id <= after) {
            continue;
        }
        cursor = Some(cursor.map_or(shown.entry.id, |cursor| cursor.max(shown.entry.id)));
        messages.push(entry_message(&spans, &shown, companion));
    }
    (messages, cursor)
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

/// One page ending at `cursor` (or at the newest message), newest last. Only
/// the page is converted into the wire shape.
pub(super) fn history(
    conversation: &Conversation,
    companion: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<w::History, Error> {
    let before = cursor
        .map(|cursor| parse_cursor(companion, cursor))
        .transpose()?;
    // A sequence counts the entries before it, so the cursor's position is
    // known without building the messages before it.
    let end = match before {
        Some(before) => shown(conversation)
            .take_while(|shown| shown.entry.index as u64 + 1 < before)
            .count(),
        None => shown(conversation).count(),
    };
    let start = end.saturating_sub(limit);
    let spans = spans(conversation);
    let items = shown(conversation)
        .skip(start)
        .take(end - start)
        .map(|shown| entry_message(&spans, &shown, companion))
        .collect::<Vec<_>>();
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

/// One conversation entry as memory reads it: Rig's own entry, its position
/// and the end of the turn it belongs to. The kernel builds these where the
/// conversation is readable; nothing is encoded for the wire.
#[derive(Clone, Debug)]
pub(crate) struct HistoryEntry {
    /// Position in the conversation; the history cursor unit.
    pub sequence: u64,
    /// Rig's entry: message, author, id and when it was made.
    pub entry: Entry,
    /// How the turn this entry belongs to ended, when it has ended.
    pub turn: Option<TurnEnd>,
    /// The request that asked for it: the user's own, or the turn's for a reply.
    pub request_id: Option<String>,
}

impl HistoryEntry {
    /// Whether it is the user's own input, not a model reply.
    pub(crate) fn is_input(&self) -> bool {
        self.borrowed().is_users_own()
    }
    /// Its id in the conversation.
    pub(crate) fn id(&self) -> MessageId {
        self.entry.id.unwrap_or_default()
    }
    /// The text the conversation shows, trimmed as the client sees it.
    pub(crate) fn text(&self) -> String {
        shown_text(&self.borrowed()).unwrap_or_default()
    }
    /// When it was made, in the RFC 3339 form the memory record keeps.
    pub(crate) fn created_at(&self) -> String {
        created_at(self.entry.made.unwrap_or_default())
    }
    fn borrowed(&self) -> EntryRef<'_> {
        EntryRef {
            index: self.sequence.saturating_sub(1) as usize,
            id: self.id(),
            message: &self.entry.message,
            author: self.entry.author.as_ref(),
            made: self.entry.made.unwrap_or_default(),
        }
    }
}

/// The entries memory reads after `after`, oldest first. Only the unprocessed
/// window is copied; earlier entries are not.
pub(super) fn history_entries_after(
    conversation: &Conversation,
    after: u64,
    limit: usize,
) -> Vec<HistoryEntry> {
    let spans = spans(conversation);
    conversation
        .entries()
        .filter(|entry| entry.index as u64 + 1 > after)
        .filter_map(|entry| history_entry(&spans, &entry, conversation))
        .take(limit)
        .collect()
}

fn history_entry(
    spans: &[TurnSpan],
    entry: &EntryRef<'_>,
    conversation: &Conversation,
) -> Option<HistoryEntry> {
    shown_text(entry)?;
    let span = span_of(spans, entry.index);
    Some(HistoryEntry {
        sequence: entry.index as u64 + 1,
        entry: conversation.entry(entry.index)?,
        turn: span.and_then(|span| span.end.clone()),
        request_id: if entry.is_users_own() {
            entry
                .author
                .and_then(|author| author.request.as_ref())
                .map(|request| request.0.clone())
        } else {
            span.and_then(|span| span.request_id.clone())
        },
    })
}

pub(super) fn message(
    conversation: &Conversation,
    companion: &str,
    id: &str,
) -> Result<w::Message, Error> {
    let spans = spans(conversation);
    shown(conversation)
        .find(|shown| shown.entry.id.to_string() == id)
        .map(|shown| entry_message(&spans, &shown, companion))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(count: u64) -> Conversation {
        Conversation(rig_core::conversation::Conversation::from_messages(
            (1..=count).map(|sequence| Message::user(format!("message-{sequence}"))),
        ))
    }

    #[test]
    fn forward_pages_continue_after_the_last_page_without_skipping() {
        let conversation = conversation(70);
        let first = history_entries_after(&conversation, 0, 64);
        assert_eq!(first.len(), 64);
        let last = first.last().unwrap().sequence;
        let rest = history_entries_after(&conversation, last, 64);
        assert_eq!(rest.len(), 6);
        assert_eq!(rest.first().unwrap().sequence, last + 1);
        assert_eq!(rest.last().unwrap().text(), "message-70");
    }

    #[test]
    fn backward_pages_end_just_before_the_cursor() {
        let conversation = conversation(70);
        let latest = history(&conversation, "c", None, 64).unwrap();
        assert_eq!(latest.items.len(), 64);
        assert_eq!(latest.items.first().unwrap().sequence, 7);
        let cursor = latest.next_cursor.unwrap();
        let older = history(&conversation, "c", Some(&cursor), 64).unwrap();
        assert_eq!(older.items.len(), 6);
        assert_eq!(older.items.first().unwrap().sequence, 1);
        assert_eq!(older.next_cursor, None);
    }
}
