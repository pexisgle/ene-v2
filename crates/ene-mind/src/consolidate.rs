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
