use super::*;
use crate::ports::llm::Policy;
use ene_api_types::host::{AuthorizationState, DependencyState, OperationAvailability};
use serde_json::json;

#[derive(Clone, Copy)]
pub struct ExecutionState {
    pub credential_present: bool,
    pub healthy: bool,
    pub stopping: bool,
    pub busy: bool,
}

/// One pure evaluation supplies admission, readiness and the UI's effective consent.
pub fn evaluate(
    configuration: &Configuration,
    companion: &str,
    connection: &ProviderConnection,
    operation: OperationKind,
    policy: Option<&Policy>,
    state: ExecutionState,
) -> OperationAvailability {
    let spec = operation.spec();
    let grants = configuration.grants.iter().filter(|g| {
        g.companion_id == companion
            && g.connection_id == connection.id
            && g.kind == "data_transfer"
            && g.purpose == spec.purpose
    });
    let authorization = match policy {
        None => AuthorizationState::Unavailable,
        Some(policy)
            if allowed(
                configuration,
                companion,
                connection,
                spec.purpose.as_str(),
                spec.data,
                policy,
            ) =>
        {
            AuthorizationState::Granted
        }
        Some(policy)
            if grants.clone().any(|g| {
                g.status == GrantStatus::Active && g.connection_fingerprint == policy.fingerprint
            }) =>
        {
            AuthorizationState::Partial
        }
        Some(_) if grants.clone().next().is_some() => AuthorizationState::Invalidated,
        _ => AuthorizationState::Missing,
    };
    let dependency = if state.stopping {
        DependencyState::Stopping
    } else if !state.healthy {
        DependencyState::RecoveryRequired
    } else if policy.is_none() {
        DependencyState::ProviderUnavailable
    } else if policy.is_some_and(|p| p.requires_credentials) && !state.credential_present {
        DependencyState::CredentialMissing
    } else if operation == OperationKind::Conversation
        && !configuration
            .companions
            .get(companion)
            .and_then(|c| c.model_assignments.companion.as_ref())
            .is_some_and(|assignment| {
                assignment.connection_id == connection.id
                    && validate_model_id(&assignment.model_id).is_ok()
            })
    {
        DependencyState::ModelUnset
    } else {
        DependencyState::Ready
    };
    let runnable = authorization == AuthorizationState::Granted
        && dependency == DependencyState::Ready
        && !state.busy;
    OperationAvailability {
        operation: operation.as_str().into(),
        purpose: spec.purpose.as_str().into(),
        data_kinds: spec.data.iter().map(|value| (*value).into()).collect(),
        authorization,
        dependency,
        busy: state.busy,
        runnable,
    }
}
pub fn connection(c: &Configuration, id: &str) -> Option<ProviderConnection> {
    c.settings.connections.iter().find(|p| p.id == id).cloned()
}

pub fn allowed(
    c: &Configuration,
    companion_id: &str,
    connection: &ProviderConnection,
    purpose: &str,
    data: &[&str],
    policy: &Policy,
) -> bool {
    c.grants.iter().any(|g| {
        g.status == GrantStatus::Active
            && g.companion_id == companion_id
            && g.kind == "data_transfer"
            && g.connection_id == connection.id
            && g.purpose.as_str() == purpose
            && g.connection_fingerprint == policy.fingerprint
            && policy
                .destinations
                .iter()
                .all(|d| g.destinations.iter().any(|v| v == d))
            && data.iter().all(|d| g.data_kinds.iter().any(|v| v == d))
    })
}

pub fn refresh(
    companion: &mut Companion,
    c: &Configuration,
    policy: Option<&Policy>,
    state: ExecutionState,
) {
    let assignment = companion.model_assignments.companion.as_ref();
    let reason = match assignment.and_then(|assignment| connection(c, &assignment.connection_id)) {
        None => Some("model_unset"),
        Some(connection) => {
            let evaluation = evaluate(
                c,
                &companion.id,
                &connection,
                OperationKind::Conversation,
                policy,
                state,
            );
            match evaluation.dependency {
                DependencyState::ModelUnset => Some("model_unset"),
                DependencyState::Stopping => Some("stopping"),
                DependencyState::RecoveryRequired => Some("storage_error"),
                DependencyState::ProviderUnavailable => Some("provider_unavailable"),
                DependencyState::CredentialMissing => Some("credential_missing"),
                _ if evaluation.authorization != AuthorizationState::Granted => {
                    Some("permission_required")
                }
                _ => None,
            }
        }
    };
    companion.availability = json!({"chat":{"ready":reason.is_none(),"reasons":reason.map(|r|vec![json!({"code":r,"message":"接続・モデル・資格情報・送信許可を設定してください"})]).unwrap_or_default()},"task":{"ready":false,"reasons":[{"code":"unsupported_capability","message":"ファイル作業は未対応です"}]},"speech":{"ready":false,"reasons":[{"code":"unsupported_capability","message":"読み上げは未対応です"}]}});
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_and_admission_share_model_validation_without_blocking_provider_checks() {
        let mut c: Configuration = serde_json::from_value(json!({
            "companions":{"c":{"id":"c","name":"ene","body":null,"voice":null,
                "mind":"日本語","model_assignments":{},"availability":{},"revision":1}},
            "settings":{"revision":1,"connections":[{"id":"p","kind":"llm",
                "adapter":"test","endpoint":"https://gateway.test","options":{},
                "credential_state":"missing","capabilities":null,"last_check":null}]},
            "grants":[{"id":"g","companion_id":"c","kind":"data_transfer",
                "connection_id":"p","destinations":["https://gateway.test"],
                "data_kinds":["conversation"],"purpose":"conversation","status":"active",
                "revision":1,"invalidation_reason":null,"connection_fingerprint":{}}]
        }))
        .unwrap();
        let connection = c.settings.connections[0].clone();
        let policy = Policy {
            revision: "v1".into(),
            fingerprint: json!({}),
            destinations: vec!["https://gateway.test".into()],
            requires_credentials: false,
            environment_variable: None,
        };
        let state = ExecutionState {
            credential_present: false,
            healthy: true,
            stopping: false,
            busy: false,
        };
        let mut check_grant = c.grants[0].clone();
        check_grant.id = "check".into();
        check_grant.purpose = GrantPurpose::ConnectionCheck;
        check_grant.data_kinds = vec!["test_input".into(), "metadata".into()];
        c.grants.push(check_grant);
        for assignment in [
            None,
            Some(ModelAssignment {
                connection_id: "other".into(),
                model_id: "m".into(),
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: " \t".into(),
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m\n".into(),
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m".repeat(MAX_MODEL_ID_BYTES + 1),
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m".into(),
            }),
        ] {
            let valid = assignment
                .as_ref()
                .is_some_and(|a| a.connection_id == "p" && a.model_id == "m");
            c.companions
                .get_mut("c")
                .unwrap()
                .model_assignments
                .companion = assignment;
            let evaluation = evaluate(
                &c,
                "c",
                &connection,
                OperationKind::Conversation,
                Some(&policy),
                state,
            );
            assert_eq!(evaluation.authorization, AuthorizationState::Granted);
            assert_eq!(evaluation.runnable, valid);
            assert_eq!(
                evaluation.dependency,
                if valid {
                    DependencyState::Ready
                } else {
                    DependencyState::ModelUnset
                }
            );
            let mut companion = c.companions["c"].clone();
            refresh(&mut companion, &c, Some(&policy), state);
            assert_eq!(companion.availability["chat"]["ready"], valid);
            for operation in [OperationKind::ConnectionCheck, OperationKind::ModelList] {
                assert!(evaluate(&c, "c", &connection, operation, Some(&policy), state).runnable);
            }
        }
    }
    #[test]
    fn permission_covers_all_destinations_data_purpose_and_implementation() {
        let mut c:Configuration=serde_json::from_value(json!({"companions":{"c":{"id":"c","name":"ene","body":null,"voice":null,"mind":"日本語","model_assignments":{},"availability":{},"revision":1}},"settings":{"revision":1,"connections":[]},"grants":[{"id":"g","companion_id":"c","kind":"data_transfer","connection_id":"p","destinations":["https://gateway.test","processor"],"data_kinds":["conversation"],"purpose":"conversation","status":"active","revision":1,"invalidation_reason":null,"connection_fingerprint":{"implementation":"v1"}}]})).unwrap();
        let p = ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "plugin".into(),
            endpoint: "https://gateway.test".into(),
            options: json!({}),
            credential_state: "missing".into(),
            capabilities: None,
            last_check: None,
        };
        let mut policy = Policy {
            revision: "v1".into(),
            fingerprint: json!({"implementation":"v1"}),
            destinations: vec!["https://gateway.test".into(), "processor".into()],
            requires_credentials: true,
            environment_variable: None,
        };
        assert!(allowed(
            &c,
            "c",
            &p,
            "conversation",
            &["conversation"],
            &policy
        ));
        assert!(!allowed(
            &c,
            "c",
            &p,
            "connection_check",
            &["conversation"],
            &policy
        ));
        assert!(!allowed(
            &c,
            "c",
            &p,
            "conversation",
            &["metadata"],
            &policy
        ));
        c.grants[0].destinations.pop();
        assert!(!allowed(
            &c,
            "c",
            &p,
            "conversation",
            &["conversation"],
            &policy
        ));
        c.grants[0].destinations.push("processor".into());
        policy.fingerprint = json!({"implementation":"v2"});
        assert!(!allowed(
            &c,
            "c",
            &p,
            "conversation",
            &["conversation"],
            &policy
        ));
    }
}
