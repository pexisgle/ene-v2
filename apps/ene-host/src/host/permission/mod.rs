//! Approval requests, saved policy rules, free-text policies and the
//! decision the Gate applies.
mod approval;
mod decision;
mod gate;
#[cfg(test)]
mod prompt_tests;
mod resolve;
#[cfg(test)]
mod tests;

pub(crate) use approval::{ApprovalRequest, RuleSave, answer, decide, refuse, submit_with_outcome};
#[cfg(test)]
pub(crate) use approval::{resolve_approval, submit};
#[cfg(test)]
pub(crate) use ene_policy::PolicyEffect;
pub(crate) use ene_policy::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, PolicyRule, canonicalise,
    extract_target, hard_denied, hard_denied_action,
};
#[cfg(test)]
pub(crate) use gate::ResolveApprovalArgs;
pub(crate) use gate::{
    Approvals, Approved, CallBinding, GatePlugin, issue_hold, notify_approval_card,
};
