//! Wire conversions for permission approval and policy types.

use crate::domain;
use ene_rpc as w;

impl From<w::ApprovalStatus> for domain::ApprovalStatus {
    fn from(value: w::ApprovalStatus) -> Self {
        match value {
            w::ApprovalStatus::Unspecified => Self::Unspecified,
            w::ApprovalStatus::Pending => Self::Pending,
            w::ApprovalStatus::Approved => Self::Approved,
            w::ApprovalStatus::Denied => Self::Denied,
            w::ApprovalStatus::Expired => Self::Expired,
        }
    }
}

impl From<domain::ApprovalDecision> for w::ApprovalDecision {
    fn from(value: domain::ApprovalDecision) -> Self {
        match value {
            domain::ApprovalDecision::Unspecified => Self::Unspecified,
            domain::ApprovalDecision::Approve => Self::Approve,
            domain::ApprovalDecision::AlwaysApprove => Self::AlwaysApprove,
            domain::ApprovalDecision::Deny => Self::Deny,
        }
    }
}

impl From<w::ApprovalRequest> for domain::ApprovalRequest {
    fn from(value: w::ApprovalRequest) -> Self {
        Self {
            request_id: value.request_id,
            session_id: value.session_id,
            run_id: value.run_id,
            action_payload_hash: value.action_payload_hash,
            tool_name: value.tool_name,
            normalized_arguments: value.normalized_arguments,
            status: value.status.as_known().map_or_default(Into::into),
            created_at: value.created_at,
            client_id: value.client_id,
            capability: value.capability,
        }
    }
}

impl From<w::PolicyEffect> for domain::PolicyEffect {
    fn from(value: w::PolicyEffect) -> Self {
        match value {
            w::PolicyEffect::Unspecified => Self::Unspecified,
            w::PolicyEffect::Allow => Self::Allow,
            w::PolicyEffect::Deny => Self::Deny,
            w::PolicyEffect::Confirm => Self::Confirm,
            w::PolicyEffect::AskLlm => Self::AskLlm,
        }
    }
}

impl From<domain::PolicyEffect> for w::PolicyEffect {
    fn from(value: domain::PolicyEffect) -> Self {
        match value {
            domain::PolicyEffect::Unspecified => Self::Unspecified,
            domain::PolicyEffect::Allow => Self::Allow,
            domain::PolicyEffect::Deny => Self::Deny,
            domain::PolicyEffect::Confirm => Self::Confirm,
            domain::PolicyEffect::AskLlm => Self::AskLlm,
        }
    }
}

impl From<w::PolicyRule> for domain::PolicyRule {
    fn from(value: w::PolicyRule) -> Self {
        Self {
            id: value.id,
            tool_name: value.tool_name,
            pattern: value.pattern,
            effect: value.effect.as_known().map_or_default(Into::into),
            created_at: value.created_at,
        }
    }
}

impl From<domain::PolicyRule> for w::PolicyRule {
    fn from(value: domain::PolicyRule) -> Self {
        Self {
            id: value.id,
            tool_name: value.tool_name,
            pattern: value.pattern,
            effect: w::PolicyEffect::from(value.effect).into(),
            created_at: value.created_at,
            ..Default::default()
        }
    }
}

impl From<w::PolicyText> for domain::PolicyText {
    fn from(value: w::PolicyText) -> Self {
        Self {
            id: value.id,
            text: value.text,
            created_at: value.created_at,
        }
    }
}

impl From<domain::PolicyText> for w::PolicyText {
    fn from(value: domain::PolicyText) -> Self {
        Self {
            id: value.id,
            text: value.text,
            created_at: value.created_at,
            ..Default::default()
        }
    }
}

impl From<domain::PolicyRulesUpdate> for w::UpdatePolicyRulesRequest {
    fn from(value: domain::PolicyRulesUpdate) -> Self {
        Self {
            add_or_update: value.add_or_update.into_iter().map(Into::into).collect(),
            remove_ids: value.remove_ids,
            ..Default::default()
        }
    }
}

impl From<domain::PolicyTextsUpdate> for w::UpdatePolicyTextsRequest {
    fn from(value: domain::PolicyTextsUpdate) -> Self {
        Self {
            add_or_update: value.add_or_update.into_iter().map(Into::into).collect(),
            remove_ids: value.remove_ids,
            ..Default::default()
        }
    }
}
