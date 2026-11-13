//! Episodic memory: ownership, sharing, recall scoring and the semantics of
//! forgetting (docs/design/mind.md §3-§4).
//!
//! Forgetting is never a delete or a boolean flag: it lowers `importance` and
//! lets the scoring function make the memory rarer on its own.
use super::super::db::epoch;

/// A memory owned by another companion and shared with every companion.
pub(crate) const SHARED_AUDIENCE: &str = "*";
pub(crate) const MAX_IMPORTANCE: u8 = 10;
/// Pinned memories (`importance == 10`) are injected within this budget.
pub(crate) const PINNED_BUDGET_CHARS: usize = 4_000;

/// Why a recall runs. Automatic recall skips what the user asked to forget;
/// an explicit question about the past may reach everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecallMode {
    Auto,
    Explicit,
}

impl RecallMode {
    pub(crate) fn minimum_importance(self) -> u8 {
        match self {
            Self::Auto => 2,
            Self::Explicit => 0,
        }
    }
}

/// One stored memory as the host sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Memory {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) audiences: Vec<String>,
    pub(crate) content: String,
    pub(crate) importance: u8,
    pub(crate) occurred_at: Option<String>,
    pub(crate) recorded_at: String,
    pub(crate) last_accessed_at: Option<String>,
    pub(crate) memory_revision: u64,
    pub(crate) content_revision: u64,
    pub(crate) emotion: Option<String>,
    pub(crate) source_context: serde_json::Value,
}

impl Memory {
    /// Whether `companion` may recall this memory. The owner always may.
    pub(crate) fn visible_to(&self, companion: &str) -> bool {
        self.owner == companion
            || self
                .audiences
                .iter()
                .any(|audience| audience == SHARED_AUDIENCE || audience == companion)
    }
}

/// A recall candidate with the score components kept apart for the model.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scored {
    pub(crate) memory: Memory,
    pub(crate) score: f64,
    pub(crate) relevance: f64,
}

/// Recall score from docs/design/mind.md §4.1: a decay coefficient by
/// importance times the weighted relevance, importance and recency.
pub(crate) fn score(
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
    let days = epoch(touched)
        .map(|then| ((now_epoch - then) as f64 / 86_400.0).max(0.0))
        .unwrap_or(0.0);
    let recency = (-0.05 * days).exp();
    decay * (0.5 * relevance + 0.3 * importance + 0.2 * recency)
}

/// Reciprocal Rank Fusion over the ranked lists that exist. Semantic ranking
/// is currently absent (no embedding connection), so the normalisation uses
/// the lists present rather than a hypothetical second list.
pub(crate) fn fuse(lists: &[&[String]], k: f64) -> Vec<(String, f64)> {
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
pub(crate) fn tokens(query: &str) -> Vec<String> {
    query
        .split(|character: char| character.is_whitespace() || is_punctuation(character))
        .map(|token| token.trim())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(importance: u8) -> Memory {
        Memory {
            id: "m1".into(),
            owner: "ene".into(),
            audiences: vec![SHARED_AUDIENCE.into()],
            content: "the user prefers tea".into(),
            importance,
            occurred_at: None,
            recorded_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: None,
            memory_revision: 1,
            content_revision: 1,
            emotion: None,
            source_context: serde_json::json!({}),
        }
    }

    #[test]
    fn forgetting_lowers_the_score_without_deleting() {
        let now = epoch("2026-01-02T00:00:00Z").unwrap();
        let important = score(1.0, 10, None, None, "2026-01-01T00:00:00Z", now);
        let forgotten = score(1.0, 0, None, None, "2026-01-01T00:00:00Z", now);
        assert!(important > forgotten);
        assert_eq!(forgotten, 0.0);
        assert!(memory(1).visible_to("other"));
    }

    #[test]
    fn sharing_filters_and_photocopies_are_not_leaked() {
        let mut private = memory(5);
        private.audiences = vec![];
        assert!(private.visible_to("ene"));
        assert!(!private.visible_to("other"));
        let shared = memory(5);
        assert!(shared.visible_to("other"));
    }

    #[test]
    fn fusion_ranks_the_top_of_each_list() {
        let first = vec!["a".to_owned(), "b".to_owned()];
        let fused = fuse(&[first.as_slice()], 60.0);
        assert_eq!(fused[0].0, "a");
        assert!((fused[0].1 - 1.0).abs() < 1e-9);
        assert!(fused[1].1 < fused[0].1);
        assert!(tokens("hello world").len() == 2);
        assert!(tokens("犬 AI").is_empty());
        assert_eq!(tokens("東京都 の").len(), 1);
    }
}
