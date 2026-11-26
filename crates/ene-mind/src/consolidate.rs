//! Consolidation domain types and citation guard.
use crate::{State, StateUpdate};
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

/// Everything one extraction returns: the facts of the interval, and the mind
/// the interval moved (a part is absent when nothing changed it).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Extraction {
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub state: Option<StateUpdate>,
}

/// The extraction system prompt: facts only, separate records, never revive
/// something the user asked to forget, and never invent a change of mind.
pub const EXTRACTION_SYSTEM_PROMPT: &str = "あなたはコンパニオンの経験記憶を整理する抽出器です。\
会話から長期記憶として残す価値のある事実だけを抽出します。\
推測・願望・ロールプレイ上の虚構を事実として保存してはいけません。\
既存記憶と同一の事実は新規作成せず、supersedes に既存IDを指定します。\
importance 1以下（忘却済み）の既存記憶を高重要度で復活させてはいけません。\
cited_memory_ids には、提示された「注入済み記憶ID」に含まれるIDだけを書きます。\
心境や成長が実際に動いた場合だけ state に、現在の心の該当部分を更新した完全な形で書きます。\
出力は指定されたJSONスキーマのみです。";

/// Renders the extraction prompt. It carries the transcript, the candidate
/// memories with their revisions, the ids the host injected and the mind the
/// interval may move.
pub fn extraction_prompt(
    turns: &[Turn],
    context: &[MemoryContext],
    injected: &[String],
    mind: &State,
) -> String {
    let mut prompt = String::from("以下から長期記憶にする事実を抽出してください。\n\n");
    prompt.push_str("既存記憶（id / revision / importance / 内容）:\n");
    if context.is_empty() {
        prompt.push_str("- なし\n");
    }
    for memory in context {
        prompt.push_str(&format!(
            "- id={} revision={} importance={} content={}\n",
            memory.id, memory.revision, memory.importance, memory.content
        ));
    }
    prompt.push_str("\n注入済み記憶ID（cited_memory_ids に書けるのはこれだけ）:\n");
    if injected.is_empty() {
        prompt.push_str("- なし\n");
    }
    for id in injected {
        prompt.push_str(&format!("- {id}\n"));
    }
    prompt.push_str(&format!(
        "\n現在の心（state を書くときはこの形に合わせる）: 感情={} 関係={} 傾向={} 懸念={}\n",
        mind.emotion.json, mind.relationships.json, mind.tendencies.json, mind.concerns.json
    ));
    prompt.push_str("\n会話（確定済みターン）:\n");
    for turn in turns {
        prompt.push_str(&format!("[turn {}]\n{}", turn.turn_id, turn.text));
    }
    prompt.push_str(
        "\n各 fact は内容ごとに分割してください。scope は非公開なら空文字、\
共有なら \"shared\" です。心境や成長が動いていない区間では state を省略してください。",
    );
    prompt
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
