//! Pure permission approval, policy rule, and policy text domain types.

use crate::{PolicyEffect, PolicyRule, PolicyText};

/// Prefix used for per-capability workspace file confirmation policy rules.
pub const SKIP_FILE_CONFIRMATION_PREFIX: &str = "desktop.skip-file-confirmation:";

/// Policy rule ID enabling automatic approval across all tool calls.
pub const SKIP_TOOL_CONFIRMATION_ID: &str = "desktop.skip-tool-confirmation";

/// Approval request lifecycle status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// The host did not report a status.
    #[default]
    Unspecified,
    /// Waiting on a person to answer.
    Pending,
    /// A person allowed the operation.
    Approved,
    /// A person refused the operation.
    Denied,
    /// Nobody answered in time, so the operation did not run.
    Expired,
}

/// Pending or resolved host approval request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Id of this approval request.
    pub request_id: String,
    /// The agent session that raised the request.
    pub session_id: String,
    /// The run that is waiting on it.
    pub run_id: String,
    /// SHA-256 of the action payload the person is approving.
    pub action_payload_hash: Vec<u8>,
    /// The tool the approved call would invoke.
    pub tool_name: String,
    /// Canonical JSON arguments, pinned so the approval cannot be widened.
    pub normalized_arguments: String,
    /// Where the request is in its lifecycle.
    pub status: ApprovalStatus,
    /// Creation time, as the host stamped it.
    pub created_at: String,
    /// The client the action targets, when the action is a device operation.
    pub client_id: Option<String>,
    /// The capability the action invokes, when it names one.
    pub capability: Option<String>,
}

/// Batch update payload for structured policy rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyRulesUpdate {
    /// Rules to add, or to replace by id.
    pub add_or_update: Vec<PolicyRule>,
    /// Ids of rules to delete.
    pub remove_ids: Vec<String>,
}

/// Batch update payload for free-text policy rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyTextsUpdate {
    /// Texts to add, or to replace by id.
    pub add_or_update: Vec<PolicyText>,
    /// Ids of texts to delete.
    pub remove_ids: Vec<String>,
}

/// Returns true if `rule` is one of the desktop's per-capability file skip rules.
pub fn is_file_confirmation_rule(rule: &PolicyRule, workspace_capabilities: &[&str]) -> bool {
    rule.id
        .strip_prefix(SKIP_FILE_CONFIRMATION_PREFIX)
        .is_some_and(|capability| workspace_capabilities.contains(&capability))
}

/// Builds a policy rule update toggling the desktop's global tool-confirmation bypass.
pub fn tool_confirmation_update(skip: bool, workspace_capabilities: &[&str]) -> PolicyRulesUpdate {
    let mut update = PolicyRulesUpdate {
        remove_ids: workspace_capabilities
            .iter()
            .map(|capability| format!("{SKIP_FILE_CONFIRMATION_PREFIX}{capability}"))
            .collect(),
        add_or_update: Vec::new(),
    };
    if skip {
        update.add_or_update.push(PolicyRule {
            id: SKIP_TOOL_CONFIRMATION_ID.into(),
            tool_name: "*".into(),
            pattern: "*".into(),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        });
    } else {
        update.remove_ids.push(SKIP_TOOL_CONFIRMATION_ID.into());
    }
    update
}
