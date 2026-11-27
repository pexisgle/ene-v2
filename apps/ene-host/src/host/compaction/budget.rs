//! Estimate and edit the model's outgoing request view.
use super::{History, NOTICE, is_summary_entry};
use rig_core::catalog::ModelSpec;
use rig_core::completion::{CompletionRequest, Message};
use rig_core::conversation::{Author, Entry, MessageId};
use rig_core::message::{ToolResultContent, UserContent};
use rig_memory::{
    ATTACHMENT_TOKENS, Anchor, HeuristicTokenCounter,
    recent::{TokenBudget, view},
    request_estimate,
};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

pub(super) fn message_tokens<'a>(messages: impl IntoIterator<Item = &'a Message>) -> u64 {
    let counter = HeuristicTokenCounter::default();
    messages
        .into_iter()
        .map(|message| counter.count(message) as u64)
        .fold(0, u64::saturating_add)
}

/// Identifies the sent prefix before the provider's measured usage can be reused.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestStart {
    fingerprint: u64,
    messages: usize,
}

fn fingerprint(request: &FitRequest<'_>, history: &History, messages: usize) -> u64 {
    let mut hash = DefaultHasher::new();
    request.model.hash(&mut hash);
    serde_json::to_string(request.prefix)
        .unwrap_or_default()
        .hash(&mut hash);
    for entry in history.entries().take(messages) {
        (entry.key(), std::ptr::from_ref(entry.message).addr()).hash(&mut hash);
    }
    hash.finish()
}

impl FitRequest<'_> {
    pub(crate) fn start(&self, history: &History) -> RequestStart {
        let messages = history.messages().len();
        RequestStart {
            fingerprint: fingerprint(self, history, messages),
            messages,
        }
    }
}

pub(crate) fn input_budget(spec: &ModelSpec, reserved_output: Option<u64>) -> u64 {
    let input = spec.input_limit().map_or(128_000, u64::from);
    let context = spec.context_window.map_or(128_000, u64::from);
    let output = reserved_output.unwrap_or_else(|| spec.output_limit().map_or(4_096, u64::from));
    input
        .min(context.saturating_sub(output))
        .saturating_sub(input / 10)
}

pub(crate) struct FitRequest<'a> {
    pub(crate) model: &'a str,
    /// The normalized outgoing request with only its fixed prompt and tools.
    pub(crate) prefix: &'a CompletionRequest,
    pub(crate) anchor: Option<&'a Anchor>,
    pub(crate) sent: Option<RequestStart>,
    pub(crate) budget: u64,
    pub(crate) turn_start: Option<MessageId>,
}

pub(crate) fn fit(history: &mut History, request: &FitRequest<'_>) -> bool {
    let FitRequest {
        model: _,
        prefix,
        anchor,
        sent,
        budget,
        turn_start,
    } = *request;
    let fixed = request_estimate(prefix);
    let tokens = |history: &History| {
        let reported = anchor.zip(sent).and_then(|(anchor, sent)| {
            let at = history.index_of(anchor.through)?;
            let follows = at == sent.messages || at + 1 == sent.messages;
            let same = follows && fingerprint(request, history, sent.messages) == sent.fingerprint;
            let count = anchor.usage.context_tokens().filter(|_| same)?;
            Some(count.saturating_add(message_tokens(
                history.messages().iter().skip(at + 1).map(Arc::as_ref),
            )))
        });
        reported.unwrap_or_else(|| {
            fixed.saturating_add(message_tokens(history.messages().iter().map(Arc::as_ref)))
        })
    };
    if tokens(history) <= budget {
        return true;
    }
    let active = turn_start.map(|start| history.start_of(start));
    let older_than = active.unwrap_or_else(|| history.messages().len().saturating_sub(1));
    if clear_tool_outputs(history, older_than) && tokens(history) <= budget {
        return true;
    }
    let limit = active.unwrap_or_else(|| last_turn(history));
    let notice = Message::system(NOTICE);
    let keep = budget
        .saturating_sub(fixed)
        .saturating_sub(message_tokens([&notice]));
    let current_tokens = message_tokens(history.messages()[limit..].iter().map(Arc::as_ref));
    let summary = history
        .entries()
        .next()
        .filter(|entry| is_summary_entry(Some(*entry)));
    let summary_tokens = summary.map_or(0, |entry| message_tokens([entry.message]));
    let retain_summary = summary.is_some() && summary_tokens.saturating_add(current_tokens) <= keep;
    let head = usize::from(retain_summary);
    let keep = if retain_summary {
        keep.saturating_sub(summary_tokens)
    } else {
        keep
    };
    let at = cut(history, keep, head, limit)
        .and_then(|id| history.index_of(id))
        .unwrap_or(usize::from(summary.is_some()));
    if at > head {
        let by = Author::plugin("ene-host");
        let notice = Entry::new(notice).by(by.clone());
        history.splice(head..at, [notice], by);
    }
    tokens(history) <= budget
}

/// The id of the first message of the newest window whose messages fit
/// `budget`: a request boundary, never before `head`, never after the newest
/// request.
pub(super) fn cut(history: &History, budget: u64, head: usize, limit: usize) -> Option<MessageId> {
    let messages = history.messages();
    let kept = view(
        messages,
        &TokenBudget::new(usize::try_from(budget).unwrap_or(usize::MAX)),
    )
    .len();
    let start = messages.len() - kept;
    let at = history
        .entries()
        .skip(start)
        .find(rig_core::conversation::EntryRef::is_request)
        .map_or(start, |entry| entry.index)
        .min(limit)
        .max(head);
    if at > head { history.id(at) } else { None }
}

/// The index of the newest message that asks something: the turn the model is
/// answering, which a window may never cut.
fn last_turn(history: &History) -> usize {
    let newest = history
        .entries()
        .rev()
        .find(rig_core::conversation::EntryRef::is_request);
    newest.map_or(history.messages().len().saturating_sub(1), |entry| {
        entry.index
    })
}

const TOOL_OUTPUT_NOTICE: &str =
    "[output cleared to fit the context window; run the tool again if needed]";

/// Keep tool-result identity and errors intact, including the latest message.
fn clear_tool_outputs(history: &mut History, older_than: usize) -> bool {
    let mut changed = false;
    history.edit_each(Author::plugin("ene-host"), |entry| {
        let Message::User { content } = entry.message else {
            return None;
        };
        if entry.index >= older_than {
            return None;
        }
        let mut edited = None;
        for (index, item) in content.iter().enumerate() {
            let UserContent::ToolResult(result) = item else {
                continue;
            };
            if result.is_error || result.content.iter().map(tool_output_tokens).sum::<usize>() < 500
            {
                continue;
            }
            let copy = edited.get_or_insert_with(|| content.clone());
            if let Some(UserContent::ToolResult(result)) = copy.get_mut(index) {
                result.content = vec![ToolResultContent::text(TOOL_OUTPUT_NOTICE)];
            }
        }
        changed |= edited.is_some();
        edited.map(|content| Message::User { content })
    });
    changed
}

fn tool_output_tokens(part: &ToolResultContent) -> usize {
    match part {
        ToolResultContent::Text(text) => text.text.len().div_ceil(4),
        ToolResultContent::Json { value } => rig_core::json_utils::json_len(value).div_ceil(4),
        ToolResultContent::Image(_) => ATTACHMENT_TOKENS,
    }
}
