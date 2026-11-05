use super::*;
use crate::llm::Policy;
use ene_rpc::OperationAvailability;
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
        None => "unavailable",
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
            "granted"
        }
        Some(policy)
            if grants.clone().any(|g| {
                g.status == GrantStatus::Active && g.connection_fingerprint == policy.fingerprint
            }) =>
        {
            "partial"
        }
        Some(_) if grants.clone().next().is_some() => "invalidated",
        _ => "missing",
    };
    let dependency = if state.stopping {
        "stopping"
    } else if !state.healthy {
        "recovery_required"
    } else if policy.is_none() {
        "provider_unavailable"
    } else if policy.is_some_and(|p| p.requires_credentials) && !state.credential_present {
        "credential_missing"
    } else if matches!(
        operation,
        OperationKind::Conversation | OperationKind::Task | OperationKind::Consolidation
    ) && !configuration
        .companions
        .get(companion)
        .and_then(|c| assignment(&c.model_assignments, operation))
        .is_some_and(|assignment| {
            assignment.connection_id == connection.id
                && validate_model_id(&assignment.model_id).is_ok()
        })
    {
        "model_unset"
    } else {
        "ready"
    };
    let runnable = authorization == "granted" && dependency == "ready" && !state.busy;
    OperationAvailability {
        operation: operation.as_str().into(),
        purpose: spec.purpose.as_str().into(),
        data_kinds: spec.data.iter().map(|value| (*value).into()).collect(),
        authorization: authorization.into(),
        dependency: dependency.into(),
        busy: state.busy,
        runnable,
        ..Default::default()
    }
}
/// The model slot an operation is allowed to use.
fn assignment(
    assignments: &ModelAssignments,
    operation: OperationKind,
) -> Option<&ModelAssignment> {
    match operation {
        OperationKind::Conversation | OperationKind::Consolidation => {
            assignments.companion.as_option()
        }
        OperationKind::Task => assignments.task.as_option(),
        OperationKind::ConnectionCheck | OperationKind::ModelList | OperationKind::Embedding => {
            None
        }
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

/// The companion's capability document. `chat_reason` is chat's blocking
/// reason; `None` means chat is ready.
pub fn capability_document(chat_reason: Option<&str>) -> serde_json::Value {
    let unsupported = |message: &str| json!({"ready": false, "reasons": [{"code": "unsupported_capability", "message": message}]});
    json!({"chat": {"ready": chat_reason.is_none(), "reasons": chat_reason.map(|reason| vec![json!({"code": reason, "message": "接続・モデル・資格情報・送信許可を設定してください"})]).unwrap_or_default()}, "task": unsupported("ファイル作業は未対応です"), "speech": unsupported("読み上げは未対応です")})
}

pub fn refresh(
    companion: &mut Companion,
    c: &Configuration,
    policy: Option<&Policy>,
    state: ExecutionState,
) {
    let assignment = companion.model_assignments.companion.as_option();
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
            match evaluation.dependency.as_str() {
                "model_unset" => Some("model_unset"),
                "stopping" => Some("stopping"),
                "recovery_required" => Some("storage_error"),
                "provider_unavailable" => Some("provider_unavailable"),
                "credential_missing" => Some("credential_missing"),
                _ if evaluation.authorization != "granted" => Some("permission_required"),
                _ => None,
            }
        }
    };
    companion.availability = ene_rpc::JsonDocument::from(capability_document(reason)).into();
}
#[cfg(test)]
mod tests {
    use super::*;
    fn configuration() -> Configuration {
        Configuration {
            companions: [(
                "c".into(),
                Companion {
                    id: "c".into(),
                    name: "ene".into(),
                    mind: "日本語".into(),
                    revision: 1,
                    model_assignments: ModelAssignments::default().into(),
                    ..Default::default()
                },
            )]
            .into(),
            settings: Settings {
                revision: 1,
                ..Default::default()
            },
            grants: vec![Grant {
                id: "g".into(),
                companion_id: "c".into(),
                kind: "data_transfer".into(),
                connection_id: "p".into(),
                destinations: vec!["https://gateway.test".into()],
                data_kinds: vec!["conversation".into()],
                purpose: GrantPurpose::Conversation,
                status: GrantStatus::Active,
                revision: 1,
                invalidation_reason: None,
                connection_fingerprint: json!({}),
            }],
            credential_refs: Default::default(),
        }
    }
    #[test]
    fn readiness_and_admission_share_model_validation_without_blocking_provider_checks() {
        let mut c = configuration();
        c.settings.connections.push(ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "test".into(),
            endpoint: "https://gateway.test".into(),
            credential_state: "missing".into(),
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        });
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
                ..Default::default()
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: " \t".into(),
                ..Default::default()
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m\n".into(),
                ..Default::default()
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m".repeat(MAX_MODEL_ID_BYTES + 1),
                ..Default::default()
            }),
            Some(ModelAssignment {
                connection_id: "p".into(),
                model_id: "m".into(),
                ..Default::default()
            }),
        ] {
            let valid = assignment
                .as_ref()
                .is_some_and(|a| a.connection_id == "p" && a.model_id == "m");
            c.companions
                .get_mut("c")
                .unwrap()
                .model_assignments
                .get_or_insert_default()
                .companion = assignment.into();
            let evaluation = evaluate(
                &c,
                "c",
                &connection,
                OperationKind::Conversation,
                Some(&policy),
                state,
            );
            assert_eq!(evaluation.authorization, "granted");
            assert_eq!(evaluation.runnable, valid);
            assert_eq!(
                evaluation.dependency,
                if valid { "ready" } else { "model_unset" }
            );
            let mut companion = c.companions["c"].clone();
            refresh(&mut companion, &c, Some(&policy), state);
            let availability: serde_json::Value =
                serde_json::from_str(&companion.availability.json).unwrap();
            assert_eq!(availability["chat"]["ready"], valid);
            for operation in [OperationKind::ConnectionCheck, OperationKind::ModelList] {
                assert!(evaluate(&c, "c", &connection, operation, Some(&policy), state).runnable);
            }
        }
    }
    #[test]
    fn memory_purpose_is_not_authorized_by_a_task_grant() {
        let mut c = configuration();
        let connection = ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "test".into(),
            endpoint: "https://gateway.test".into(),
            ..Default::default()
        };
        c.settings.connections.push(connection.clone());
        let policy = Policy {
            revision: "v1".into(),
            fingerprint: json!({}),
            destinations: vec!["https://gateway.test".into()],
            requires_credentials: false,
            environment_variable: None,
        };
        let mut task = c.grants[0].clone();
        task.purpose = GrantPurpose::Task;
        task.data_kinds = vec!["task".into()];
        c.grants = vec![task];
        assert!(!allowed(
            &c,
            "c",
            &connection,
            "memory",
            &["memory"],
            &policy
        ));
        let mut memory = c.grants[0].clone();
        memory.purpose = GrantPurpose::Memory;
        memory.data_kinds = vec!["memory".into()];
        c.grants = vec![memory];
        assert!(allowed(
            &c,
            "c",
            &connection,
            "memory",
            &["memory"],
            &policy
        ));
        assert!(!allowed(&c, "c", &connection, "task", &["task"], &policy));
        assert_eq!(
            OperationKind::Embedding.spec().purpose,
            GrantPurpose::Memory
        );
        assert_eq!(
            OperationKind::Consolidation.spec().purpose,
            GrantPurpose::Memory
        );
    }

    #[test]
    fn task_inference_requires_the_task_assignment() {
        let mut c = configuration();
        c.settings.connections.push(ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "test".into(),
            endpoint: "https://gateway.test".into(),
            ..Default::default()
        });
        let connection = c.settings.connections[0].clone();
        let policy = Policy {
            revision: "v1".into(),
            fingerprint: json!({}),
            destinations: vec!["https://gateway.test".into()],
            requires_credentials: false,
            environment_variable: None,
        };
        let mut task_grant = c.grants[0].clone();
        task_grant.id = "task".into();
        task_grant.purpose = GrantPurpose::Task;
        task_grant.data_kinds = vec!["task".into()];
        c.grants.push(task_grant);
        let state = ExecutionState {
            credential_present: false,
            healthy: true,
            stopping: false,
            busy: false,
        };
        let missing = evaluate(
            &c,
            "c",
            &connection,
            OperationKind::Task,
            Some(&policy),
            state,
        );
        assert_eq!(missing.dependency, "model_unset");
        assert!(!missing.runnable);
        c.companions
            .get_mut("c")
            .unwrap()
            .model_assignments
            .get_or_insert_default()
            .task = Some(ModelAssignment {
            connection_id: "p".into(),
            model_id: "task-model".into(),
            ..Default::default()
        })
        .into();
        let ready = evaluate(
            &c,
            "c",
            &connection,
            OperationKind::Task,
            Some(&policy),
            state,
        );
        assert_eq!(ready.dependency, "ready");
        assert_eq!(ready.authorization, "granted");
        assert!(ready.runnable);
    }

    #[test]
    fn permission_covers_all_destinations_data_purpose_and_implementation() {
        let mut c = configuration();
        c.grants[0].destinations.push("processor".into());
        c.grants[0].connection_fingerprint = json!({"implementation":"v1"});
        let p = ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "plugin".into(),
            endpoint: "https://gateway.test".into(),
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            credential_state: "missing".into(),
            capabilities: Default::default(),
            last_check: Default::default(),
            ..Default::default()
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
