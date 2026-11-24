//! Episodic memory domain types, recall scoring, RRF fusion, and update payloads.
use crate::{companion::State, json::JsonDoc};
use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The scope every companion recalls. A memory's `scope` is either this value
/// or a companion id, so no other value can widen visibility.
pub const SHARED_SCOPE: &str = "shared";
/// Maximum memory importance level.
pub const MAX_IMPORTANCE: u8 = 10;
/// Pinned memories (`importance == 10`) are injected within this budget.
pub const PINNED_BUDGET_CHARS: usize = 3_000;

/// The one visibility rule: a memory is visible when it lives in the reader's
/// own scope or in the shared pool (`docs/design/mind.md §3.3`).
pub fn in_scope(scope: &str, companion: &str) -> bool {
    scope == companion || scope == SHARED_SCOPE
}

/// Why a recall runs. Automatic recall skips what the user asked to forget;
/// an explicit question about the past may reach everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallMode {
    Auto,
    Explicit,
}

impl RecallMode {
    /// Minimum importance threshold included for this recall mode.
    pub fn minimum_importance(self) -> u8 {
        match self {
            Self::Auto => 2,
            Self::Explicit => 0,
        }
    }
}

/// One stored memory as the host sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub id: String,
    pub scope: String,
    pub content: String,
    pub importance: u8,
    pub occurred_at: Option<String>,
    pub recorded_at: String,
    pub last_accessed_at: Option<String>,
    pub memory_revision: u64,
    pub content_revision: u64,
    pub emotion: Option<String>,
    pub source_context: Value,
}

impl Memory {
    /// Whether `companion` may recall this memory.
    pub fn visible_to(&self, companion: &str) -> bool {
        in_scope(&self.scope, companion)
    }
}

/// A recall candidate with the score components kept apart for the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    pub memory: Memory,
    pub score: f64,
    pub relevance: f64,
}

fn parse_epoch(text: &str) -> Option<i64> {
    text.parse::<Timestamp>().ok().map(Timestamp::as_second)
}

/// Recall score from `docs/design/mind.md §4.1`: a decay coefficient by
/// importance times the weighted relevance, importance and recency.
pub fn score(
    relevance: f64,
    importance: u8,
    last_touched: Option<&str>,
    occurred_at: Option<&str>,
    recorded_at: &str,
    now_epoch: i64,
) -> f64 {
    let importance = f64::from(importance) / 10.0;
    let decay = (importance / 0.3).min(1.0);
    let relevance = relevance.clamp(0.0, 1.0);
    let touched = last_touched.or(occurred_at).unwrap_or(recorded_at);
    let days =
        parse_epoch(touched).map_or(0.0, |then| ((now_epoch - then) as f64 / 86_400.0).max(0.0));
    let recency = (-0.05 * days).exp();
    decay * (0.5 * relevance + 0.3 * importance + 0.2 * recency)
}

/// Reciprocal Rank Fusion over the ranked lists present.
pub fn fuse(lists: &[&[String]], k: f64) -> Vec<(String, f64)> {
    use std::collections::HashMap;
    let mut fused: HashMap<&str, f64> = HashMap::new();
    for list in lists {
        for (index, id) in list.iter().enumerate() {
            *fused.entry(id.as_str()).or_default() += 1.0 / (k + (index + 1) as f64);
        }
    }
    let ceiling = lists.iter().filter(|list| !list.is_empty()).count().max(1) as f64 / (k + 1.0);
    let mut ranked: Vec<(String, f64)> = fused
        .into_iter()
        .map(|(id, value)| (id.to_owned(), (value / ceiling).clamp(0.0, 1.0)))
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked
}

/// Splits a query into FTS5 tokens: whitespace and punctuation only. Tokens
/// shorter than three characters cannot match a trigram index, so they are
/// dropped and the caller falls back to `LIKE`.
pub fn tokens(query: &str) -> Vec<String> {
    query
        .split(|character: char| character.is_whitespace() || is_punctuation(character))
        .map(str::trim)
        .filter(|token| token.chars().count() >= 3)
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect()
}

fn is_punctuation(character: char) -> bool {
    matches!(
        character,
        ',' | '.'
            | '、'
            | '。'
            | '!'
            | '?'
            | '！'
            | '？'
            | ':'
            | ';'
            | '「'
            | '」'
            | '('
            | ')'
            | '（'
            | '）'
            | '['
            | ']'
            | '【'
            | '】'
            | '・'
            | '…'
            | '/'
            | '\\'
            | '"'
            | '\''
            | '“'
            | '”'
    )
}

/// What a caller may write when a memory is first recorded.
#[derive(Clone, Debug)]
pub struct MemoryRecord {
    pub id: String,
    pub scope: String,
    pub content: String,
    pub importance: u8,
    pub occurred_at: Option<String>,
    pub source_context: Value,
}

/// One element of `mind_update`: a new memory, a correction or a forget.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMemory {
    /// Correct an existing memory instead of recording a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The memory text; a correction may omit it to change only importance.
    #[serde(default)]
    pub content: String,
    /// 0-10; 10 is injected every turn, 0-1 are effectively forgotten.
    #[serde(default)]
    pub importance: Option<u8>,
    #[serde(default)]
    pub occurred_at: Option<String>,
    /// The revision that was read; a stale value is a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// An existing memory this record supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Forget the addressed memory by lowering its importance to zero.
    #[serde(default, skip_serializing_if = "is_false")]
    pub forget: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The mind state patch of a `mind_update`; each part is a schema'd document.
/// A part the caller leaves out keeps its stored value.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateUpdate {
    #[serde(default)]
    pub emotion: Option<Value>,
    #[serde(default)]
    pub relationships: Option<Value>,
    #[serde(default)]
    pub tendencies: Option<Value>,
    #[serde(default)]
    pub concerns: Option<Value>,
}

impl StateUpdate {
    /// Applies present state patches onto `state`.
    pub fn apply(&self, state: &mut State) {
        if let Some(emotion) = &self.emotion {
            state.emotion = JsonDoc::new("ene.emotion.v1", emotion.clone());
        }
        if let Some(relationships) = &self.relationships {
            state.relationships = JsonDoc::new("ene.relationships.v1", relationships.clone());
        }
        if let Some(tendencies) = &self.tendencies {
            state.tendencies = JsonDoc::new("ene.tendencies.v1", tendencies.clone());
        }
        if let Some(concerns) = &self.concerns {
            state.concerns = JsonDoc::new("ene.concerns.v1", concerns.clone());
        }
    }
}

/// The mind state half of a `mind_update`, already serialised.
#[derive(Clone, Debug)]
pub struct MindStateChange {
    pub expected_revision: u64,
    pub state: String,
}

/// Everything a `mind_update` commits atomically.
#[derive(Clone, Debug)]
pub struct MindUpdate {
    pub operation_id: String,
    pub companion_id: String,
    pub request_hash: Vec<u8>,
    pub memories: Vec<NewMemory>,
    pub state: Option<MindStateChange>,
}

/// The result of a hybrid recall: the ranked items plus whether a semantic
/// candidate list was actually used.
#[derive(Clone, Debug)]
pub struct RecallOutcome {
    pub items: Vec<Scored>,
    pub semantic: bool,
}

/// Actions a user or operator may perform on an existing memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryActionKind {
    Forget,
    Remember,
    Share,
    Unshare,
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// A list of memory ids, which may contain repeats the way a real
    /// recall pass can.
    fn ids() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[a-z]{1,4}", 0..8)
    }

    proptest! {
        /// Fusing one list twice is the same as fusing it once: the function
        /// is a ranking rule, not an accumulator.
        #[test]
        fn fusing_is_idempotent(a in ids()) {
            let once = fuse(&[&a], 60.0);
            let twice = fuse(&[&a, &a], 60.0);
            let order = |v: &[(String, f64)]| v.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
            prop_assert_eq!(order(&once), order(&twice));
        }

        /// Scores are finite and sorted high to low; a NaN would make the sort
        /// non-deterministic and silently reorder the recall.
        #[test]
        fn scores_are_finite_and_sorted(lists in prop::collection::vec(ids(), 1..4)) {
            let refs: Vec<&[String]> = lists.iter().map(Vec::as_slice).collect();
            let fused = fuse(&refs, 60.0);
            for (_, score) in &fused {
                prop_assert!(score.is_finite(), "score must be finite, got {score}");
                prop_assert!(*score > 0.0, "a listed id always scores above zero");
            }
            let scores: Vec<f64> = fused.iter().map(|(_, s)| *s).collect();
            let mut sorted = scores.clone();
            sorted.sort_by(|a, b| b.partial_cmp(a).expect("finite"));
            prop_assert_eq!(scores, sorted, "results must be ordered by score");
        }

        /// Every id that appeared anywhere appears exactly once in the output.
        #[test]
        fn output_is_the_union_of_inputs(lists in prop::collection::vec(ids(), 1..4)) {
            let refs: Vec<&[String]> = lists.iter().map(Vec::as_slice).collect();
            let fused = fuse(&refs, 60.0);
            let mut out: Vec<&String> = fused.iter().map(|(id, _)| id).collect();
            let before = out.len();
            out.sort();
            out.dedup();
            prop_assert_eq!(out.len(), before, "no id may repeat in the output");

            for id in lists.iter().flatten() {
                prop_assert!(out.contains(&id), "{id} vanished from the union");
            }
        }
    }
}
