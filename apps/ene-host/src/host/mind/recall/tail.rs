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
use ene_mind::{Memory, MemoryVersion, RecallCandidate, RecallMode};
use ene_store::util::now;
use rig_ecs::prelude::{Conversation, MessageId};
use std::collections::HashSet;
use std::fmt::Write as _;

#[derive(Debug)]
pub(super) struct RenderedTail {
    context: String,
    pinned: Vec<SelectedMemory>,
    recalled: Vec<SelectedMemory>,
}

#[derive(Debug)]
struct SelectedMemory {
    version: MemoryVersion,
    content: String,
}

pub(super) struct TailPayload {
    pub(super) text: String,
    pub(super) memories: Vec<MemoryVersion>,
}

const PINNED_HEADER: &str = "常に覚えていること:\n";
const RECALL_HEADER: &str = "今の話に関係する記憶:\n";

impl RenderedTail {
    pub(super) fn fallback() -> Self {
        Self {
            context: opening(),
            pinned: Vec::new(),
            recalled: Vec::new(),
        }
    }

    pub(super) fn payload(
        &self,
        store: &Store,
        companion: &str,
    ) -> crate::host::db::Result<TailPayload> {
        let current = store.memory_repo().current_revisions(
            companion,
            self.pinned
                .iter()
                .chain(&self.recalled)
                .map(|item| &item.version),
        )?;
        let mut current = current.into_iter();
        let mut text = self.context.clone();
        let mut memories = Vec::new();
        for (header, items) in [
            (PINNED_HEADER, &self.pinned),
            (RECALL_HEADER, &self.recalled),
        ] {
            let mut heading = Some(header);
            for (item, available) in items.iter().zip(current.by_ref().take(items.len())) {
                if available {
                    if let Some(header) = heading.take() {
                        text.push_str(header);
                    }
                    let _written = writeln!(text, "- {}", item.content);
                    memories.push(item.version.clone());
                }
            }
        }
        text.push_str(TAIL_CLOSING);
        Ok(TailPayload { text, memories })
    }
}

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

/// Reserves context space before selecting complete memory lines.
struct ContextBuilder {
    budget_chars: usize,
    used_chars: usize,
    buf: String,
}

impl ContextBuilder {
    fn new(total_budget_chars: usize) -> Self {
        let closing_chars = TAIL_CLOSING.chars().count();
        Self {
            budget_chars: total_budget_chars.saturating_sub(closing_chars),
            used_chars: 0,
            buf: String::new(),
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

    fn select(
        &mut self,
        header: &str,
        source: impl IntoIterator<Item = Memory>,
        skip_oversized: bool,
    ) -> Vec<SelectedMemory> {
        let mut selected = Vec::new();
        let header_chars = header.chars().count();
        for memory in source {
            let mut needed = memory.content.chars().count() + 3; // "- " and newline
            if selected.is_empty() {
                needed += header_chars;
            }
            if needed > self.remaining_chars() {
                if skip_oversized {
                    continue;
                }
                break;
            }
            self.used_chars += needed;
            selected.push(SelectedMemory {
                version: MemoryVersion::from(&memory),
                content: memory.content,
            });
        }
        selected
    }

    fn finish(self, pinned: Vec<SelectedMemory>, recalled: Vec<SelectedMemory>) -> RenderedTail {
        RenderedTail {
            context: self.buf,
            pinned,
            recalled,
        }
    }
}

pub(super) struct DynamicTailQuery<'a> {
    pub(super) store: &'a Store,
    pub(super) perception: &'a Perception,
    pub(super) companion: &'a str,
    pub(super) query: Option<&'a str>,
    pub(super) embeddings: &'a Embeddings,
    pub(super) credentials: Option<&'a Credentials>,
}

pub(super) struct TailRenderRequest<'a> {
    pub(super) store: &'a Store,
    pub(super) perception: &'a Perception,
    pub(super) companion: &'a str,
    pub(super) query: Option<&'a str>,
    #[cfg(test)]
    pub(super) semantic: &'a [RecallCandidate],
    pub(super) hook_notes: &'a [String],
    pub(super) budget_chars: usize,
}

pub(super) async fn dynamic_tail(
    req: DynamicTailQuery<'_>,
) -> crate::host::db::Result<RenderedTail> {
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
    let render = TailRenderRequest {
        store: req.store,
        perception: req.perception,
        companion: req.companion,
        query: req.query,
        #[cfg(test)]
        semantic: &[],
        hook_notes: &hook_notes,
        budget_chars: TAIL_BUDGET_CHARS,
    };
    let (mut builder, pinned) = prepare_tail(&render)?;
    let Some(pinned) = pinned else {
        return Ok(builder.finish(Vec::new(), Vec::new()));
    };
    let Some(query) = render.query else {
        return Ok(builder.finish(pinned, Vec::new()));
    };
    let excluded = pinned_ids(&pinned);
    let mode = RecallMode::AutoExcept(&excluded);
    let semantic = embedding::semantic_candidates(
        req.store,
        req.embeddings,
        req.credentials,
        req.companion,
        query,
        mode,
    )
    .await;
    let recalled = select_recall(&mut builder, &render, query, mode, &semantic)?;
    Ok(builder.finish(pinned, recalled))
}

fn pinned_ids(pinned: &[SelectedMemory]) -> HashSet<&str> {
    pinned.iter().map(|item| item.version.id.as_str()).collect()
}

#[cfg(test)]
pub(super) fn render_tail_full(
    req: &TailRenderRequest<'_>,
) -> crate::host::db::Result<RenderedTail> {
    let (mut builder, pinned) = prepare_tail(req)?;
    let Some(pinned) = pinned else {
        return Ok(builder.finish(Vec::new(), Vec::new()));
    };
    let recalled = if let Some(query) = req.query {
        let excluded = pinned_ids(&pinned);
        select_recall(
            &mut builder,
            req,
            query,
            RecallMode::AutoExcept(&excluded),
            req.semantic,
        )?
    } else {
        Vec::new()
    };
    Ok(builder.finish(pinned, recalled))
}

fn select_recall(
    builder: &mut ContextBuilder,
    req: &TailRenderRequest<'_>,
    query: &str,
    mode: RecallMode<'_>,
    semantic: &[RecallCandidate],
) -> crate::host::db::Result<Vec<SelectedMemory>> {
    let recalled =
        req.store
            .memory_repo()
            .recall_hybrid(req.companion, query, mode, semantic, 6)?;
    Ok(builder.select(
        RECALL_HEADER,
        recalled.items.into_iter().map(|item| item.memory),
        false,
    ))
}

fn prepare_tail(
    req: &TailRenderRequest<'_>,
) -> crate::host::db::Result<(ContextBuilder, Option<Vec<SelectedMemory>>)> {
    let &TailRenderRequest {
        store,
        perception,
        companion,
        hook_notes,
        budget_chars,
        ..
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
    let pinned = if let Some(state) = store.companion_repo().companion(companion)? {
        builder.push_priority(&format!(
            "現在の心: 感情={} 関係={} 傾向={} 懸念={}\n",
            state.state.emotion.json,
            state.state.relationships.json,
            state.state.tendencies.json,
            state.state.concerns.json
        ));
        Some(builder.select(PINNED_HEADER, store.memory_repo().pinned(companion)?, true))
    } else {
        None
    };
    Ok((builder, pinned))
}
