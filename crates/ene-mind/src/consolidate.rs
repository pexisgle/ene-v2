//! Consolidation domain types and citation guard.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// One answered turn as data: the id its window position is measured in, and
/// the text the extraction model reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub turn_id: String,
    pub text: String,
}

/// An existing memory the extraction model may merge into or supersede. The
/// revision is the CAS expectation captured before the model ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryContext {
    pub id: String,
    pub revision: u64,
    pub importance: u8,
    pub content: String,
}

/// The window one pass commits, with the material its commit needs.
#[derive(Clone, Debug)]
pub struct Batch {
    pub companion_id: String,
    /// The newest message the summary this pass runs beside stands for: the id
    /// every memory and edge of the window is stamped with.
    pub through: u64,
    pub context: Vec<MemoryContext>,
    /// Memory ids the host actually injected into those turns.
    pub injected: Vec<String>,
}

/// The result of one committed pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    pub inserted: usize,
    pub updated: usize,
}

/// One extracted fact. `scope` is empty (the recording companion's own scope)
/// or `"shared"` (every companion).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(inline, deny_unknown_fields)]
pub struct Fact {
    #[schemars(length(max = 4000))]
    pub content: String,
    #[serde(default)]
    #[schemars(range(min = 0, max = 10))]
    pub importance: Option<u8>,
    #[serde(default)]
    #[schemars(length(max = 128))]
    pub scope: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 128))]
    pub supersedes: Option<String>,
    #[serde(default)]
    #[schemars(inner(length(max = 128)))]
    pub cited_memory_ids: Vec<String>,
}

/// Host-side citation guard: a cited id counts only when the host really
/// injected it into the turn being consolidated.
pub fn validate_citations(cited: &[String], injected: &[String]) -> Vec<String> {
    let injected: HashSet<&str> = injected.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    cited
        .iter()
        .filter(|id| injected.contains(id.as_str()) && seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// Trims, bounds and deduplicates the facts a pass proposed.
///
/// A fact with no content, or one over the length budget, is dropped; the
/// remaining list is capped so one verbose answer cannot flood the store.
/// How many facts one consolidation pass may commit.
const MAX_FACTS: usize = 32;
/// How many cited memory ids one fact may carry.
const MAX_CITATIONS: usize = 64;

pub fn normalize(facts: Vec<Fact>) -> Vec<Fact> {
    let mut normalized = Vec::new();
    for mut fact in facts {
        fact.content = fact.content.trim().to_owned();
        if fact.content.is_empty() || fact.content.chars().count() > 4_000 {
            continue;
        }
        fact.importance = Some(fact.importance.unwrap_or(3).min(10));
        fact.scope = fact
            .scope
            .map(|scope| scope.trim().chars().take(128).collect())
            .filter(|scope: &String| !scope.is_empty());
        fact.supersedes = fact
            .supersedes
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        fact.cited_memory_ids = fact
            .cited_memory_ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .take(MAX_CITATIONS)
            .collect();
        normalized.push(fact);
        if normalized.len() >= MAX_FACTS {
            break;
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(content: &str) -> Fact {
        Fact {
            content: content.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn normalization_bounds_every_field_a_pass_can_supply() {
        let trimmed = normalize(vec![fact("  padded  ")]);
        assert_eq!(trimmed[0].content, "padded");
        // An absent importance defaults to the middle of the scale.
        assert_eq!(trimmed[0].importance, Some(3));

        let clamped = normalize(vec![Fact {
            importance: Some(250),
            ..fact("x")
        }]);
        assert_eq!(clamped[0].importance, Some(10));

        let scoped = normalize(vec![Fact {
            scope: Some("  ".into()),
            supersedes: Some(String::new()),
            ..fact("x")
        }]);
        assert_eq!(scoped[0].scope, None, "a blank scope is dropped");
        assert_eq!(scoped[0].supersedes, None, "a blank id is dropped");
    }

    #[test]
    fn empty_and_oversized_facts_never_reach_the_store() {
        assert_eq!(normalize(vec![fact("   ")]).len(), 0);
        assert_eq!(normalize(vec![fact(&"x".repeat(4_001))]).len(), 0);
        assert_eq!(normalize(vec![fact(&"x".repeat(4_000))]).len(), 1);
    }

    #[test]
    fn one_verbose_answer_cannot_flood_the_store() {
        let many: Vec<Fact> = (0..(MAX_FACTS * 3)).map(|_| fact("m")).collect();
        assert_eq!(normalize(many).len(), MAX_FACTS);
    }
}
