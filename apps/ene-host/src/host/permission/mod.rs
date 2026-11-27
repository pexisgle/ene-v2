//! Approval requests, saved policy rules, free-text policies and the
//! decision the Gate applies.
mod approval;
mod conversation;
mod decision;
mod external;
mod gate;
#[cfg(test)]
mod prompt_tests;
mod resolve;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use approval::submit;
pub(crate) use approval::{ApprovalRequest, RuleSave, answer, decide, refuse, submit_with_outcome};
#[cfg(test)]
pub(crate) use conversation::{ResolveApprovalArgs, resolve_approval};
pub(crate) use conversation::{consider_user_reply, user_reply_delivery};
#[cfg(test)]
pub(crate) use ene_policy::PolicyEffect;
pub(crate) use ene_policy::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, PolicyRule, canonicalise,
    extract_target, hard_denied, hard_denied_action,
};
pub(crate) use gate::{
    Approvals, Approved, CallBinding, GatePlugin, issue_hold, notify_approval_card,
};
/// Authenticates an effective invocation without exposing its credentials or arguments.
pub(crate) fn scope_tag<'a>(
    key: &[u8; 32],
    domain: &[u8],
    fields: impl IntoIterator<Item = &'a [u8]>,
) -> Option<[u8; 32]> {
    use hmac::{Hmac, KeyInit, Mac};

    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).ok()?;
    mac.update(domain);
    for field in fields {
        mac.update(&(field.len() as u64).to_be_bytes());
        mac.update(field);
    }
    Some(mac.finalize().into_bytes().into())
}
