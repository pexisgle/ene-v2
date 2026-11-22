//! Pure permission approval, policy rule, and policy text domain types.

/// Prefix used for per-capability workspace file confirmation policy rules.
pub const SKIP_FILE_CONFIRMATION_PREFIX: &str = "desktop.skip-file-confirmation:";

/// Policy rule ID enabling automatic approval across all tool calls.
pub const SKIP_TOOL_CONFIRMATION_ID: &str = "desktop.skip-tool-confirmation";

/// Approval request lifecycle status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalStatus {
    #[default]
    Unspecified,
    Pending,
    Approved,
    Denied,
    Expired,
}

/// User decision submitted for a pending approval card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalDecision {
    #[default]
    Unspecified,
    Approve,
    AlwaysApprove,
    Deny,
}

/// Pending or resolved host approval request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub action_payload_hash: Vec<u8>,
    pub tool_name: String,
    pub normalized_arguments: String,
    pub status: ApprovalStatus,
    pub created_at: String,
    pub client_id: Option<String>,
    pub capability: Option<String>,
}

/// Enforcement action for a structured policy rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PolicyEffect {
    #[default]
    Unspecified,
    Allow,
    Deny,
    Confirm,
    AskLlm,
}

/// Structured tool/capability permission rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyRule {
    pub id: String,
    pub tool_name: String,
    pub pattern: String,
    pub effect: PolicyEffect,
    pub created_at: String,
}

/// Free-text policy guideline evaluated by the decision engine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyText {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

/// Batch update payload for structured policy rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyRulesUpdate {
    pub add_or_update: Vec<PolicyRule>,
    pub remove_ids: Vec<String>,
}

/// Batch update payload for free-text policy rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyTextsUpdate {
    pub add_or_update: Vec<PolicyText>,
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
