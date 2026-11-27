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

/// Combines lexical and semantic rank, then applies the memory scoring rule.
pub fn rank_recall(lists: [Vec<Memory>; 2], now_epoch: i64, limit: usize) -> Vec<Scored> {
    const K: f64 = 60.0;
    let ceiling = lists.iter().filter(|list| !list.is_empty()).count().max(1) as f64 / (K + 1.0);
    let mut fused = std::collections::HashMap::new();
    for list in lists {
        for (index, memory) in list.into_iter().enumerate() {
            let (_, sum) = fused.entry(memory.id.clone()).or_insert((memory, 0.0));
            *sum += 1.0 / (K + (index + 1) as f64);
        }
    }
    let mut ranked: Vec<Scored> = fused
        .into_values()
        .map(|(memory, sum)| {
            let relevance = (sum / ceiling).clamp(0.0, 1.0);
            let score = score(
                relevance,
                memory.importance,
                memory.last_accessed_at.as_deref(),
                memory.occurred_at.as_deref(),
                &memory.recorded_at,
                now_epoch,
            );
            Scored {
                memory,
                score,
                relevance,
            }
        })
        .collect();
    ranked.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.memory.importance.cmp(&left.memory.importance))
            .then_with(|| left.memory.id.cmp(&right.memory.id))
    });
    ranked.truncate(limit);
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
    use std::collections::{HashMap, HashSet};

    /// A list of memory ids, which may contain repeats the way a real
    /// recall pass can.
    fn ids() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[a-z]{1,4}", 0..8)
    }

    fn records(ids: &[String]) -> Vec<Memory> {
        ids.iter()
            .map(|id| Memory {
                id: id.clone(),
                scope: "fixture".into(),
                content: id.clone(),
                importance: 6,
                occurred_at: None,
                recorded_at: "2026-11-21T00:00:00Z".into(),
                last_accessed_at: None,
                memory_revision: 1,
                content_revision: 1,
                emotion: None,
                source_context: Value::Null,
            })
            .collect()
    }

    #[test]
    fn a_match_in_both_channels_ranks_first() {
        let lexical = records(&["a".into(), "b".into()]);
        let semantic = records(&["b".into(), "c".into()]);
        assert_eq!(rank_recall([lexical, semantic], 0, 1)[0].memory.id, "b");
    }

    fn rank_separately(lists: &[Vec<Memory>; 2], now: i64) -> Vec<Scored> {
        let mut sums: HashMap<&str, f64> = HashMap::new();
        for list in lists {
            for (i, item) in list.iter().enumerate() {
                *sums.entry(&item.id).or_default() += 1.0 / (60.0 + (i + 1) as f64);
            }
        }
        let ceiling = lists.iter().filter(|list| !list.is_empty()).count().max(1) as f64 / 61.0;
        let mut seen = HashSet::new();
        let mut scored: Vec<_> = lists
            .iter()
            .flatten()
            .filter(|memory| seen.insert(memory.id.as_str()))
            .map(|memory| {
                let relevance = (sums[memory.id.as_str()] / ceiling).clamp(0.0, 1.0);
                Scored {
                    memory: memory.clone(),
                    relevance,
                    score: score(
                        relevance,
                        memory.importance,
                        memory.last_accessed_at.as_deref(),
                        memory.occurred_at.as_deref(),
                        &memory.recorded_at,
                        now,
                    ),
                }
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.memory.importance.cmp(&a.memory.importance))
                .then_with(|| a.memory.id.cmp(&b.memory.id))
        });
        scored
    }

    proptest! {
        #[test]
        fn fusing_is_idempotent(a in ids()) {
            let list = records(&a);
            let once = rank_recall([list.clone(), Vec::new()], 0, usize::MAX);
            let twice = rank_recall([list.clone(), list], 0, usize::MAX);
            let order = |v: Vec<Scored>| v.into_iter().map(|item| item.memory.id).collect::<Vec<_>>();
            prop_assert_eq!(order(once), order(twice));
        }

        #[test]
        fn scores_are_finite_and_sorted(a in ids(), b in ids()) {
            let ranked = rank_recall([records(&a), records(&b)], 0, usize::MAX);
            for item in &ranked {
                prop_assert!(item.score.is_finite());
                prop_assert!(item.relevance.is_finite() && item.relevance > 0.0);
            }
            prop_assert!(ranked.windows(2).all(|pair| pair[0].score >= pair[1].score));
        }

        #[test]
        fn output_is_the_union_of_inputs(a in ids(), b in ids()) {
            let ranked = rank_recall([records(&a), records(&b)], 0, usize::MAX);
            let actual: HashSet<_> = ranked.iter().map(|item| &item.memory.id).collect();
            prop_assert_eq!(actual.len(), ranked.len());
            prop_assert_eq!(actual, a.iter().chain(&b).collect());
        }

        #[test]
        fn merging_preserves_original_values_and_tie_breaks(a in ids(), b in ids(), limit in 0..16usize) {
            let lexical = records(&a);
            let mut semantic = records(&b);
            for item in &mut semantic {
                item.importance = 2;
                item.last_accessed_at = Some("2026-11-25T00:00:00Z".into());
            }
            let lists = [lexical, semantic];
            let now = 1_795_219_200;
            let mut expected = rank_separately(&lists, now);
            expected.truncate(limit);
            prop_assert_eq!(rank_recall(lists, now, limit), expected);
        }
    }
}
