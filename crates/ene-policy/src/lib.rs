//! Pure client capability and tool permission policy domain types and evaluation.
#![warn(missing_docs)]
// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
pub mod capability;
mod grants;
mod hard_deny;
mod risk;
pub mod rules;
#[cfg(test)]
mod rules_property_tests;
#[cfg(test)]
mod rules_tests;
mod targets;

pub use capability::{
    BUILTIN_TOOLS, COMPUTER_USE, CapabilityApproval, CapabilitySpec, WORKSPACE_CAPABILITY_PREFIX,
    approval_target, parse_schema, validate_declarations,
};
pub use grants::CapabilityGrants;
pub use hard_deny::{
    hard_denied, hard_denied_action, hard_denied_search_destination, is_host_tool,
};

/// Error raised when a pure domain value violates structural or format constraints.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// The input fails domain validation rules.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias for policy validation.
pub type Result<T> = std::result::Result<T, DomainError>;
pub use risk::RiskLevel;
pub use rules::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, MAX_POLICY_TEXT_CHARS,
    PolicyEffect, PolicyRule, PolicyText, SearchApprovalSummary, Verdict, canonical_json,
    canonicalise, decide_action, decide_capability, hex_digest,
};
mod scope;
pub use scope::{ExternalToolAction, ExternalToolSource, InvocationScope, WebSearchAction};
pub use targets::{ActionTarget, extract_target};
