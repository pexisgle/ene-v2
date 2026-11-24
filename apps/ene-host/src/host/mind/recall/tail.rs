//! Building the dynamic tail's text.
//!
//! [`TurnCause`] decides what the turn is asking for, and the rest renders the
//! clock, the user profile, perception, the companion's state, pinned memories
//! and automatic recall within a fixed character budget. The ECS side that
//! decides *when* a tail is built stays in the parent module.
use crate::host::{
    agent,
    db::Store,
    mind::{
        embedding::{self, Embeddings},
        memory::{RecallMode, Scored},
    },
    model::Credentials,
    resources::Perception,
    util::now,
};
use rig_ecs::prelude::{Conversation, MessageId};

/// The tail's opening: the clock is part of every request, companion or not.
pub(super) fn opening() -> String {
    format!(
        "---\n現在の動的コンテキスト（会話ログには保存されません）\n現在日時: {}\n",
        now()
    )
}

/// The dynamic tail a request carries: the clock, the user profile, perception,
/// the companion's state, pinned memories and automatic recall. It is injected
/// into the request view only and never saved with the conversation, so a call
/// that is not a kernel turn (a heartbeat tick) attaches the same tail an
/// ordinary turn would get. `turn` is the companion turn the automatic recall is
/// recorded under; a work run or a call that is not a turn passes `None` and
/// records nothing.
/// What triggered the current turn, derived exclusively from `conversation.asked(turn_start)`
/// so non-user turns (schedules, work reports, eye reports) never recycle a stale user message
/// from an earlier turn (`B13`, `R09`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TurnCause {
    UserMessage(String),
    EyeReportOnly,
    WorkReport(String),
    Scheduled(String),
    Other(Option<String>),
}

impl TurnCause {
    pub(super) fn classify(
        conversation: Option<&Conversation>,
        turn_start: MessageId,
        has_eye_report: bool,
    ) -> Self {
        let Some(conversation) = conversation else {
            return if has_eye_report {
                Self::EyeReportOnly
            } else {
                Self::Other(None)
            };
        };
        let asked: Vec<_> = conversation.asked(turn_start).collect();
        if let Some(text) = asked
            .iter()
            .rev()
            .find(|entry| entry.is_users_own())
            .and_then(agent::entry_text)
        {
            return Self::UserMessage(text);
        }
        if has_eye_report {
            return Self::EyeReportOnly;
        }
        for entry in asked.iter().rev() {
            let Some(text) = agent::entry_text(entry) else {
                continue;
            };
            match entry.author.map(|author| &author.kind) {
                Some(rig_core::conversation::AuthorKind::Agent) => {
                    return Self::WorkReport(text);
                }
                Some(rig_core::conversation::AuthorKind::Plugin(_))
                    if text.starts_with("[スケジュール実行:") =>
                {
                    return Self::Scheduled(text);
                }
                _ => return Self::Other(Some(text)),
            }
        }
        Self::Other(None)
    }

    pub(super) fn recall_query(&self) -> Option<&str> {
        match self {
            Self::UserMessage(text) | Self::WorkReport(text) | Self::Scheduled(text) => {
                Some(text.as_str())
            }
            Self::EyeReportOnly => None,
            Self::Other(text) => text.as_deref(),
        }
    }
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
    pub(super) semantic: &'a [String],
    pub(super) hook_notes: &'a [String],
    pub(super) turn: Option<u64>,
    pub(super) budget_chars: usize,
}

pub(crate) async fn dynamic_tail(req: DynamicTailQuery<'_>) -> crate::host::db::Result<String> {
    let semantic = match req.query {
        Some(query) => {
            embedding::semantic_ids(
                req.store,
                req.embeddings,
                req.credentials,
                req.companion,
                query,
            )
            .await
        }
        None => Vec::new(),
    };
    let providers = req.embeddings.providers();
    let hook_notes = if !providers
        .with_extension(ene_plugin::Extension::Hook)
        .is_empty()
    {
        let outcome = crate::host::providers::extensions::run_hooks(
            &providers,
            req.store,
            ene_plugin::HookEvent::PreTurn {
                companion_id: req.companion.to_owned(),
                query: req.query.map(str::to_owned),
            },
        )
        .await;
        outcome.injected_context
    } else {
        Vec::new()
    };
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
    semantic: &[String],
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
