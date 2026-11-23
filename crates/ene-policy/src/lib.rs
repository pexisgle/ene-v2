//! Pure client capability and tool permission policy domain types and evaluation.
pub mod capability;
pub mod error;
pub mod rules;
#[cfg(test)]
mod rules_property_tests;
#[cfg(test)]
mod rules_tests;

pub use capability::{
    BUILTIN_TOOLS, COMPUTER_USE, CapabilityApproval, CapabilitySpec, WORKSPACE_CAPABILITY_PREFIX,
    approval_target, validate_declarations,
};
pub use error::{PolicyError, Result};
pub use rules::{
    ActionTarget, Approval, ApprovalDecision, ApprovalStatus, CandidateAction,
    MAX_POLICY_TEXT_CHARS, PolicyEffect, PolicyRule, PolicyText, Verdict, canonical_json,
    canonicalise, decide_action, decide_capability, extract_target, hard_denied,
    hard_denied_action, hex_digest, is_host_tool,
};
