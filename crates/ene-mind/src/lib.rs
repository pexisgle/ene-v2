//! Pure domain types and scoring/validation rules for companion minds and episodic memory.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
pub mod companion;
pub mod consolidate;
pub mod embedding;
pub mod json;
pub mod memory;
pub mod prompt;

pub use companion::{Companion, Profile, Role, State, default_ene_companion, validate_id};
pub use consolidate::{
    Batch, Commit, EXTRACTION_SYSTEM_PROMPT, Extraction, Fact, MemoryContext, Turn,
    extraction_prompt, normalize, validate_citations,
};
pub use embedding::{PREP_VERSION, PendingEmbedding, dimensions_of_space, space_id};
pub use ene_domain::{ModelAssignment, ModelAssignments};
pub use json::JsonDoc;
pub use memory::{
    MAX_IMPORTANCE, Memory, MemoryActionKind, MemoryRecord, MindStateChange, MindUpdate, NewMemory,
    PINNED_BUDGET_CHARS, RecallCandidate, RecallMode, RecallOutcome, SHARED_SCOPE, Scored,
    StateUpdate, in_scope, rank_recall, score, tokens,
};

pub type Result<T> = std::result::Result<T, ene_domain::DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ene_companion_validates_and_resolves_role_fallbacks() {
        let mut companion = default_ene_companion("2026-11-21T00:00:00Z");
        assert!(companion.validate().is_ok());

        companion.assignments.companion = ModelAssignment {
            connection_id: "conn-1".into(),
            model_id: "gpt-5.4-mini".into(),
        };
        companion.assignments.task = ModelAssignment {
            connection_id: "conn-2".into(),
            model_id: "gpt-5.4".into(),
        };
        assert_eq!(
            companion.model(Role::Perception),
            Some(&companion.assignments.companion)
        );
        assert_eq!(
            companion.model(Role::Decision),
            Some(&companion.assignments.task)
        );
        assert!(companion.model(Role::Embedding).is_none());
    }

    #[test]
    fn recall_scoring_and_citation_guard_preserve_invariants() {
        let s_high = score(0.9, 8, None, None, "2026-11-21T00:00:00Z", 1_795_219_200);
        let s_low = score(0.9, 1, None, None, "2026-11-21T00:00:00Z", 1_795_219_200);
        assert!(s_high > s_low);

        assert_eq!(
            tokens("hi rust, メモリ管理!"),
            vec!["\"rust\"", "\"メモリ管理\""]
        );
        assert_eq!(
            validate_citations(&["m1".into(), "m2".into(), "m1".into()], &["m1".into()]),
            vec!["m1".to_owned()]
        );

        let space = space_id(
            "openai",
            "https://api.openai.com/v1/",
            r#"{"b":2,"a":1}"#,
            "text-embedding-3-small",
            1536,
        );
        assert_eq!(dimensions_of_space(&space), Some(1536));
        assert_eq!(dimensions_of_space("bad:v0"), None);
    }
}
