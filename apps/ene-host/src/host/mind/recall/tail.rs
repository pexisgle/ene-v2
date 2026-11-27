//! Building the dynamic tail's text.
//!
//! Renders the clock, profile, perception, mind and selected memories for the
//! current turn. The parent module applies the tail to an unsaved request view.
use crate::host::{
    agent,
    db::Store,
    mind::embedding::{self, Embeddings},
    model::Credentials,
    resources::Perception,
};
use ene_mind::{RecallCandidate, RecallMode, Scored};
use ene_store::util::now;
use rig_ecs::prelude::{Conversation, MessageId};

/// The tail's opening: the clock is part of every request, companion or not.
pub(super) fn opening() -> String {
    format!(
        "---\n現在の動的コンテキスト（会話ログには保存されません）\n現在日時: {}\n",
        now()
    )
}

/// Search the current turn's input, prioritizing the latest user message.
pub(super) fn recall_query(
    conversation: Option<&Conversation>,
    turn_start: MessageId,
    has_eye_report: bool,
) -> Option<String> {
    let asked: Vec<_> = conversation?.asked(turn_start).collect();
    let user = asked.iter().rev().find(|entry| entry.is_users_own());
    if let Some(text) = user.and_then(agent::entry_text) {
        return Some(text);
    }
    if has_eye_report {
        return None;
    }
    asked.iter().rev().find_map(agent::entry_text)
}

pub(super) const TAIL_BUDGET_CHARS: usize = 6_000;
const TAIL_CLOSING: &str = "---";

/// Assembles the dynamic context tail within a fixed character budget and tracks
/// exactly which recalled memory IDs survived budget fitting (`B14`, `R09`).
struct ContextBuilder {
    budget_chars: usize,
    used_chars: usize,
    buf: String,
    included_recalled_ids: Vec<String>,
}

impl ContextBuilder {
    fn new(total_budget_chars: usize) -> Self {
        let closing_chars = TAIL_CLOSING.chars().count();
        Self {
            budget_chars: total_budget_chars.saturating_sub(closing_chars),
            used_chars: 0,
            buf: String::new(),
            included_recalled_ids: Vec::new(),
        }
    }

    fn remaining_chars(&self) -> usize {
        self.budget_chars.saturating_sub(self.used_chars)
    }

    /// Appends priority context (clock, profile, perception, state, pinned),
    /// clipping cleanly to the remaining budget if needed.
    fn push_priority(&mut self, text: &str) {
        let rem = self.remaining_chars();
        if rem == 0 {
            return;
        }
        let count = text.chars().count();
        if count <= rem {
            self.buf.push_str(text);
            self.used_chars += count;
        } else {
            let clipped: String = text.chars().take(rem).collect();
            self.buf.push_str(&clipped);
            self.used_chars = self.budget_chars;
        }
    }

    /// Appends recalled memories one full line at a time, recording only IDs
    /// whose entire bullet fit within the remaining budget.
    fn push_recalled(&mut self, items: &[Scored]) {
        const HEADER: &str = "今の話に関係する記憶:\n";
        if items.is_empty() {
            return;
        }
        let header_chars = HEADER.chars().count();
        let mut header_written = false;
        for item in items {
            let line = format!("- {}\n", item.memory.content);
            let line_chars = line.chars().count();
            let needed = if header_written {
                line_chars
            } else {
                header_chars + line_chars
            };
            if needed > self.remaining_chars() {
                break;
            }
            if !header_written {
                self.buf.push_str(HEADER);
                self.used_chars += header_chars;
                header_written = true;
            }
            self.buf.push_str(&line);
            self.used_chars += line_chars;
            self.included_recalled_ids.push(item.memory.id.clone());
        }
    }

    fn finish(mut self) -> (String, Vec<String>) {
        self.buf.push_str(TAIL_CLOSING);
        (self.buf, self.included_recalled_ids)
    }
}

pub(crate) struct DynamicTailQuery<'a> {
    pub(crate) store: &'a Store,
    pub(crate) perception: &'a Perception,
    pub(crate) companion: &'a str,
    pub(crate) query: Option<&'a str>,
    pub(crate) embeddings: &'a Embeddings,
    pub(crate) credentials: Option<&'a Credentials>,
    pub(crate) turn: Option<u64>,
}

pub(super) struct TailRenderRequest<'a> {
    pub(super) store: &'a Store,
    pub(super) perception: &'a Perception,
    pub(super) companion: &'a str,
    pub(super) query: Option<&'a str>,
    pub(super) semantic: &'a [RecallCandidate],
    pub(super) hook_notes: &'a [String],
    pub(super) turn: Option<u64>,
    pub(super) budget_chars: usize,
}

pub(crate) async fn dynamic_tail(req: DynamicTailQuery<'_>) -> crate::host::db::Result<String> {
    let semantic = match req.query {
        Some(query) => {
            embedding::semantic_candidates(
                req.store,
                req.embeddings,
                req.credentials,
                req.companion,
                query,
                RecallMode::Auto,
            )
            .await
        }
        None => Vec::new(),
    };
    let hook_notes = crate::host::providers::extensions::run_hooks(
        &req.embeddings.providers(),
        req.store,
        ene_plugin::HookEvent::PreTurn {
            companion_id: req.companion.to_owned(),
            query: req.query.map(str::to_owned),
        },
    )
    .await
    .injected_context;
    render_tail_full(&TailRenderRequest {
        store: req.store,
        perception: req.perception,
        companion: req.companion,
        query: req.query,
        semantic: &semantic,
        hook_notes: &hook_notes,
        turn: req.turn,
        budget_chars: TAIL_BUDGET_CHARS,
    })
}

pub(super) fn render_tail(
    store: &Store,
    perception: &Perception,
    companion: &str,
    query: Option<&str>,
    semantic: &[RecallCandidate],
    turn: Option<u64>,
) -> crate::host::db::Result<String> {
    render_tail_full(&TailRenderRequest {
        store,
        perception,
        companion,
        query,
        semantic,
        hook_notes: &[],
        turn,
        budget_chars: TAIL_BUDGET_CHARS,
    })
}

pub(super) fn render_tail_full(req: &TailRenderRequest<'_>) -> crate::host::db::Result<String> {
    let &TailRenderRequest {
        store,
        perception,
        companion,
        query,
        semantic,
        hook_notes,
        turn,
        budget_chars,
    } = req;
    let mut builder = ContextBuilder::new(budget_chars);
    builder.push_priority(&opening());
    let profile = crate::host::db::memories::profile_lines(store)?;
    if !profile.is_empty() {
        builder.push_priority("ユーザー情報:\n");
        for line in profile {
            builder.push_priority(&format!("- {line}\n"));
        }
    }
    // Perception and the clock lead the tail so truncation cannot drop them:
    // the model must always know the current environment.
    if let Some(event) = perception.events.get(companion) {
        builder.push_priority(&format!("知覚: {}\n", event.summary));
    }
    if !hook_notes.is_empty() {
        builder.push_priority("プラグイン補足:\n");
        for note in hook_notes {
            builder.push_priority(&format!("- {note}\n"));
        }
    }
    if let Some(state) = store.companion_repo().companion(companion)? {
        builder.push_priority(&format!(
            "現在の心: 感情={} 関係={} 傾向={} 懸念={}\n",
            state.state.emotion.json,
            state.state.relationships.json,
            state.state.tendencies.json,
            state.state.concerns.json
        ));
        let pinned = store.memory_repo().pinned(companion)?;
        if !pinned.is_empty() {
            builder.push_priority("常に覚えていること:\n");
            for memory in pinned {
                builder.push_priority(&format!("- {}\n", memory.content));
            }
        }
        if let Some(query) = query {
            let recalled = store.memory_repo().recall_hybrid(
                companion,
                query,
                RecallMode::Auto,
                semantic,
                6,
            )?;
            builder.push_recalled(&recalled.items);
        }
    }
    let (tail, included_ids) = builder.finish();
    if let Some(turn) = turn
        && !included_ids.is_empty()
    {
        // Persisted ONLY for recalled memories that actually survived budget
        // fitting, so the citation guard never trusts a truncated memory
        // (docs/design/mind.md §4.2, B14).
        store
            .memory_repo()
            .record_injections(companion, &turn.to_string(), &included_ids)?;
    }
    Ok(tail)
}
