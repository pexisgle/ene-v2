//! Pure client capability and tool permission policy domain types and evaluation.
pub mod capability;
pub mod error;
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
    approval_target, validate_declarations,
};
pub use error::{PolicyError, Result};
pub use hard_deny::{hard_denied, hard_denied_action, is_host_tool};
pub use risk::RiskLevel;
pub use rules::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, MAX_POLICY_TEXT_CHARS,
    PolicyEffect, PolicyRule, PolicyText, Verdict, canonical_json, canonicalise, decide_action,
    decide_capability, hex_digest,
};
pub use targets::{ActionTarget, extract_target};
