//! Shared conversions between Connect messages and domain models.

use crate as wire;
use ene_domain as domain;

macro_rules! shared_fields {
    ($message:path, $model:path { $($field:ident),+ $(,)? }) => {
        impl From<$message> for $model {
            fn from(value: $message) -> Self {
                Self { $($field: value.$field,)+ }
            }
        }

        impl From<&$message> for $model {
            fn from(value: &$message) -> Self {
                Self { $($field: value.$field.clone(),)+ }
            }
        }

        impl From<$model> for $message {
            fn from(value: $model) -> Self {
                Self { $($field: value.$field,)+ ..Default::default() }
            }
        }

        impl From<&$model> for $message {
            fn from(value: &$model) -> Self {
                Self { $($field: value.$field.clone(),)+ ..Default::default() }
            }
        }
    };
}

shared_fields!(wire::Failure, domain::Failure { code, message });
shared_fields!(
    wire::PolicyText,
    domain::PolicyText {
        id,
        text,
        created_at
    }
);
shared_fields!(
    wire::ConnectionEndpoint,
    domain::Endpoint {
        adapter,
        endpoint,
        options_schema,
        options
    }
);
shared_fields!(
    wire::ModelAssignment,
    domain::ModelAssignment {
        connection_id,
        model_id
    }
);

impl From<Option<domain::ModelAssignment>> for wire::AssignmentPatch {
    fn from(assignment: Option<domain::ModelAssignment>) -> Self {
        use wire::assignment_patch::Change;
        let change = match assignment {
            Some(value) => Change::Set(Box::new(value.into())),
            None => Change::Clear(Box::default()),
        };
        Self {
            change: Some(change),
            ..Default::default()
        }
    }
}

impl From<&wire::assignment_patch::Change> for Option<domain::ModelAssignment> {
    fn from(change: &wire::assignment_patch::Change) -> Self {
        match change {
            wire::assignment_patch::Change::Set(value) => Some(value.as_ref().into()),
            wire::assignment_patch::Change::Clear(_) => None,
        }
    }
}

impl From<wire::ModelAssignments> for domain::ModelAssignments {
    fn from(value: wire::ModelAssignments) -> Self {
        Self {
            companion: value.companion.into_option().map_or_default(Into::into),
            task: value.task.into_option().map_or_default(Into::into),
            perception: value.perception.into_option().map(Into::into),
            compaction: value.compaction.into_option().map(Into::into),
            consolidation: value.consolidation.into_option().map(Into::into),
            decision: value.decision.into_option().map(Into::into),
            embedding: value.embedding.into_option().map(Into::into),
        }
    }
}

impl From<domain::ModelAssignments> for wire::ModelAssignments {
    fn from(value: domain::ModelAssignments) -> Self {
        Self {
            companion: Some(value.companion.into()).into(),
            task: Some(value.task.into()).into(),
            perception: value.perception.map(Into::into).into(),
            compaction: value.compaction.map(Into::into).into(),
            consolidation: value.consolidation.map(Into::into).into(),
            decision: value.decision.map(Into::into).into(),
            embedding: value.embedding.map(Into::into).into(),
            ..Default::default()
        }
    }
}

impl From<wire::ConnectionCheck> for domain::Check {
    fn from(value: wire::ConnectionCheck) -> Self {
        Self {
            healthy: value.healthy,
            checked_at: value.checked_at,
            failure: value.failure.into_option().map(Into::into),
            capabilities: value.capabilities,
            resources: value.resources,
        }
    }
}

impl From<&domain::Check> for wire::ConnectionCheck {
    fn from(value: &domain::Check) -> Self {
        Self {
            healthy: value.healthy,
            checked_at: value.checked_at.clone(),
            failure: value.failure.as_ref().map(Into::into).into(),
            capabilities: value.capabilities.clone(),
            resources: value.resources.clone(),
            ..Default::default()
        }
    }
}

impl From<&domain::ModelAssignments> for wire::ModelAssignments {
    fn from(value: &domain::ModelAssignments) -> Self {
        value.clone().into()
    }
}

impl TryFrom<wire::PolicyRule> for domain::PolicyRule {
    type Error = domain::DomainError;

    fn try_from(value: wire::PolicyRule) -> Result<Self, Self::Error> {
        use domain::PolicyEffect as Model;
        use wire::PolicyEffect as Wire;
        let effect = match value.effect.as_known() {
            Some(Wire::Allow) => Model::Allow,
            Some(Wire::Deny) => Model::Deny,
            Some(Wire::Confirm) => Model::Confirm,
            Some(Wire::AskLlm) => Model::AskLlm,
            Some(Wire::Unspecified) | None => {
                return Err(domain::DomainError::Invalid(
                    "a policy effect is required".into(),
                ));
            }
        };
        Ok(Self {
            id: value.id,
            tool_name: value.tool_name,
            pattern: value.pattern,
            effect,
            created_at: value.created_at,
        })
    }
}

impl From<&domain::PolicyRule> for wire::PolicyRule {
    fn from(value: &domain::PolicyRule) -> Self {
        use domain::PolicyEffect as Model;
        use wire::PolicyEffect as Wire;
        let effect = match value.effect {
            Model::Allow => Wire::Allow,
            Model::Deny => Wire::Deny,
            Model::Confirm => Wire::Confirm,
            Model::AskLlm => Wire::AskLlm,
        };
        Self {
            id: value.id.clone(),
            tool_name: value.tool_name.clone(),
            pattern: value.pattern.clone(),
            effect: effect.into(),
            created_at: value.created_at.clone(),
            ..Default::default()
        }
    }
}

macro_rules! state_conversion {
    ($wire:ty, $model:ty, $label:literal, $($variant:ident),+ $(,)?) => {
        impl TryFrom<$wire> for $model {
            type Error = domain::DomainError;

            fn try_from(value: $wire) -> Result<Self, Self::Error> {
                match value {
                    $(<$wire>::$variant => Ok(Self::$variant),)+
                    <$wire>::Unspecified => Err(domain::DomainError::Invalid($label.into())),
                }
            }
        }

        impl From<$model> for $wire {
            fn from(value: $model) -> Self {
                match value {
                    $(<$model>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

state_conversion!(
    wire::ApprovalStatus,
    ene_policy::ApprovalStatus,
    "an approval status is required",
    Pending,
    Approved,
    Denied,
    Expired,
);
state_conversion!(
    wire::WorkStatus,
    domain::WorkStatus,
    "a work status is required",
    Idle,
    Active,
    Closed
);
state_conversion!(
    wire::RunStatus,
    domain::RunState,
    "a run state is required",
    Starting,
    Running,
    WaitingInput,
    Completed,
    Cancelled,
    Failed,
);

impl TryFrom<wire::WorkSession> for domain::WorkSession {
    type Error = domain::DomainError;

    fn try_from(value: wire::WorkSession) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            companion_id: domain::CompanionId::parse(value.companion_id)?,
            title: value.title,
            status: value
                .status
                .as_known()
                .unwrap_or(wire::WorkStatus::Unspecified)
                .try_into()?,
            created_at: value.created_at,
            updated_at: value.updated_at,
            client_id: (!value.client_id.is_empty()).then_some(value.client_id),
        })
    }
}

impl From<&domain::WorkSession> for wire::WorkSession {
    fn from(value: &domain::WorkSession) -> Self {
        Self {
            id: value.id.clone(),
            companion_id: value.companion_id.to_string(),
            title: value.title.clone(),
            status: wire::WorkStatus::from(value.status).into(),
            created_at: value.created_at.clone(),
            updated_at: value.updated_at.clone(),
            client_id: value.client_id.clone().unwrap_or_default(),
            ..Default::default()
        }
    }
}

impl TryFrom<wire::Activation> for domain::Activation {
    type Error = domain::DomainError;

    fn try_from(value: wire::Activation) -> Result<Self, Self::Error> {
        Ok(Self {
            companion_id: domain::CompanionId::parse(value.companion_id)?,
            client_id: value.client_id,
        })
    }
}

impl From<&domain::Activation> for wire::Activation {
    fn from(value: &domain::Activation) -> Self {
        Self {
            companion_id: value.companion_id.to_string(),
            client_id: value.client_id.clone(),
            ..Default::default()
        }
    }
}
