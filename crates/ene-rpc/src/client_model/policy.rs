//! Wire conversions for permission approval and policy types.

use crate as w;
use ene_domain::client_view as domain;

from_variants! {
    domain::ApprovalDecision => w::ApprovalDecision [
        Unspecified, Approve, AlwaysApprove, Deny,
    ]
}

from_fields! {
    w::ApprovalRequest => domain::ApprovalRequest {
        request_id, session_id, run_id, action_payload_hash, tool_name,
        normalized_arguments,
        status.as_known().and_then(|status| status.try_into().ok()),
        created_at, client_id, capability, external_destination,
        search_summary.into_option().map(Into::into),
    }
}

from_fields! {
    w::SearchApprovalSummary => ene_policy::SearchApprovalSummary {
        provider, connection_name, connection_id, query, limit,
    }
}

from_fields! {
    &ene_policy::SearchApprovalSummary => w::SearchApprovalSummary {
        provider.clone(), connection_name.clone(), connection_id.clone(), query.clone(), limit,
        ..Default::default()
    }
}

from_fields! {
    domain::PolicyRulesUpdate => w::UpdatePolicyRulesRequest {
        add_or_update.iter().map(Into::into).collect(),
        remove_ids,
        ..Default::default()
    }
}

from_fields! {
    domain::PolicyTextsUpdate => w::UpdatePolicyTextsRequest {
        add_or_update.into_iter().map(Into::into).collect(),
        remove_ids,
        ..Default::default()
    }
}
