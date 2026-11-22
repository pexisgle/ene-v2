//! Anti-corruption layer (ACL) conversions between `ene_rpc` wire types and pure domain models.

pub mod companion;
pub mod connection;
pub mod policy;

use crate::domain;
use ene_rpc as w;

impl From<w::Failure> for domain::Failure {
    fn from(value: w::Failure) -> Self {
        Self {
            code: value.code,
            message: value.message,
        }
    }
}

impl From<domain::Failure> for w::Failure {
    fn from(value: domain::Failure) -> Self {
        Self {
            code: value.code,
            message: value.message,
            ..Default::default()
        }
    }
}

impl From<w::JsonDocument> for domain::JsonDocument {
    fn from(value: w::JsonDocument) -> Self {
        Self {
            schema: value.schema,
            json: value.json,
        }
    }
}

impl From<domain::JsonDocument> for w::JsonDocument {
    fn from(value: domain::JsonDocument) -> Self {
        Self {
            schema: value.schema,
            json: value.json,
            ..Default::default()
        }
    }
}

impl From<w::HostHealth> for domain::HostHealth {
    fn from(value: w::HostHealth) -> Self {
        Self {
            ready: value.ready,
            recovery_required: value.recovery_required,
            stopping: value.stopping,
        }
    }
}

impl From<w::Client> for domain::ClientInfo {
    fn from(value: w::Client) -> Self {
        Self {
            id: value.id,
            name: value.name,
            platform: value.platform,
            capabilities: value.capabilities,
            granted_capabilities: value.granted_capabilities,
            revision: value.revision,
            last_seen: value.last_seen,
            connected: value.connected,
            active_companions: value.active_companions,
        }
    }
}

impl From<w::Activation> for domain::Activation {
    fn from(value: w::Activation) -> Self {
        Self {
            companion_id: value.companion_id,
            client_id: value.client_id,
        }
    }
}

impl From<w::WorkStatus> for domain::WorkStatus {
    fn from(value: w::WorkStatus) -> Self {
        match value {
            w::WorkStatus::Unspecified => Self::Unspecified,
            w::WorkStatus::Idle => Self::Idle,
            w::WorkStatus::Active => Self::Active,
            w::WorkStatus::Closed => Self::Closed,
        }
    }
}

impl From<w::RunStatus> for domain::RunStatus {
    fn from(value: w::RunStatus) -> Self {
        match value {
            w::RunStatus::Unspecified => Self::Unspecified,
            w::RunStatus::Starting => Self::Starting,
            w::RunStatus::Running => Self::Running,
            w::RunStatus::WaitingInput => Self::WaitingInput,
            w::RunStatus::Completed => Self::Completed,
            w::RunStatus::Cancelled => Self::Cancelled,
            w::RunStatus::Failed => Self::Failed,
        }
    }
}

impl From<w::WorkSession> for domain::WorkSession {
    fn from(value: w::WorkSession) -> Self {
        Self {
            id: value.id,
            companion_id: value.companion_id,
            title: value.title,
            status: value.status.as_known().map(Into::into).unwrap_or_default(),
            created_at: value.created_at,
            updated_at: value.updated_at,
            client_id: value.client_id,
        }
    }
}

impl From<w::WorkEvent> for domain::WorkEvent {
    fn from(value: w::WorkEvent) -> Self {
        let detail = value.detail.map(|detail| match detail {
            w::work_event::Detail::StatusChanged(changed) => {
                domain::WorkEventDetail::StatusChanged(
                    changed
                        .status
                        .as_known()
                        .map(Into::into)
                        .unwrap_or_default(),
                )
            }
            w::work_event::Detail::RunStarted(_) => domain::WorkEventDetail::RunStarted,
            w::work_event::Detail::RunFinished(finished) => domain::WorkEventDetail::RunFinished {
                state: finished
                    .state
                    .as_known()
                    .map(Into::into)
                    .unwrap_or_default(),
                failure: finished.failure.into_option().map(Into::into),
                pending_request_id: finished.pending_request_id,
            },
        });
        Self {
            work_id: value.work_id,
            run_id: value.run_id,
            detail,
        }
    }
}

impl From<w::Settings> for domain::Settings {
    fn from(value: w::Settings) -> Self {
        Self {
            revision: value.revision,
            connections: value.connections.into_iter().map(Into::into).collect(),
            updated_at: value.updated_at,
            policy_texts: value.policy_texts.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<w::Snapshot> for domain::Snapshot {
    fn from(value: w::Snapshot) -> Self {
        Self {
            server_version: value.server_version,
            health: value.health.into_option().map(Into::into),
            settings: value.settings.into_option().map(Into::into),
            companions: value.companions.into_iter().map(Into::into).collect(),
            work_sessions: value.work_sessions.into_iter().map(Into::into).collect(),
            active_companion_id: value.active_companion_id,
            pending_approvals: value
                .pending_approvals
                .into_iter()
                .map(Into::into)
                .collect(),
            clients: value.clients.into_iter().map(Into::into).collect(),
            policy_rules: value.policy_rules.into_iter().map(Into::into).collect(),
            activations: value.activations.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<w::HostEvent> for domain::HostEvent {
    fn from(value: w::HostEvent) -> Self {
        Self {
            kind: value.kind,
            payload: value.payload.into_option().map(Into::into),
        }
    }
}

impl From<w::MindUpdated> for domain::MindUpdated {
    fn from(value: w::MindUpdated) -> Self {
        Self {
            companion_id: value.companion_id,
            profile_revision: value.profile_revision,
            state_revision: value.state_revision,
            updated_at: value.updated_at,
        }
    }
}

impl From<w::MessageDelta> for domain::MessageDelta {
    fn from(value: w::MessageDelta) -> Self {
        Self {
            companion_id: value.companion_id,
            stream_id: value.stream_id,
            delta: value.delta,
        }
    }
}

impl From<w::Notification> for domain::HostNotification {
    fn from(value: w::Notification) -> Self {
        let event = value.event.map(|event| match event {
            w::notification::Event::Health(health) => {
                domain::NotificationEvent::Health((*health).into())
            }
            w::notification::Event::Companion(companion) => {
                domain::NotificationEvent::Companion(Box::new((*companion).into()))
            }
            w::notification::Event::MindUpdated(updated) => {
                domain::NotificationEvent::MindUpdated((*updated).into())
            }
            w::notification::Event::MessageCreated(created) => {
                match created.message.into_option() {
                    Some(message) => {
                        domain::NotificationEvent::MessageCreated(Box::new(message.into()))
                    }
                    None => domain::NotificationEvent::Other,
                }
            }
            w::notification::Event::MessageDelta(delta) => {
                domain::NotificationEvent::MessageDelta((*delta).into())
            }
            w::notification::Event::TurnEvent(turn) => {
                domain::NotificationEvent::TurnEvent(Box::new((*turn).into()))
            }
            w::notification::Event::WorkEvent(work) => {
                domain::NotificationEvent::WorkEvent((*work).into())
            }
            w::notification::Event::ApprovalRequested(requested) => {
                domain::NotificationEvent::ApprovalRequested(
                    requested
                        .request
                        .into_option()
                        .map(Into::into)
                        .map(Box::new),
                )
            }
            w::notification::Event::PolicyRulesUpdated(updated) => {
                domain::NotificationEvent::PolicyRulesUpdated(
                    updated.rules.into_iter().map(Into::into).collect(),
                )
            }
            w::notification::Event::PolicyTextsUpdated(updated) => {
                domain::NotificationEvent::PolicyTextsUpdated(
                    updated.texts.into_iter().map(Into::into).collect(),
                )
            }
            w::notification::Event::SettingsUpdated(updated) => {
                domain::NotificationEvent::SettingsUpdated(
                    updated.settings.into_option().map(Into::into).map(Box::new),
                )
            }
            w::notification::Event::ClientUpdated(updated) => {
                domain::NotificationEvent::ClientUpdated(
                    updated.client.into_option().map(Into::into).map(Box::new),
                )
            }
            w::notification::Event::Generic(generic) => {
                domain::NotificationEvent::Generic((*generic).into())
            }
            w::notification::Event::InitialState(snapshot) => {
                domain::NotificationEvent::InitialState(Box::new((*snapshot).into()))
            }
            w::notification::Event::ResyncRequired(resync) => {
                domain::NotificationEvent::ResyncRequired {
                    reason: resync.reason,
                }
            }
            w::notification::Event::ActivationUpdated(activation) => {
                domain::NotificationEvent::ActivationUpdated((*activation).into())
            }
            w::notification::Event::DeviceOperation(_)
            | w::notification::Event::DeviceOperationResult(_) => domain::NotificationEvent::Other,
        });
        Self {
            kind: value.kind,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_acl_preserves_connection_template_and_policy_round_trips() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("FOO".to_owned(), "bar".to_owned());
        let domain_input = domain::ConnectionInput {
            id: "mcp-1".into(),
            name: "MCP".into(),
            enabled: true,
            expected_revision: 4,
            kind: domain::ConnectionKind::Mcp,
            config: Some(domain::ConnectionConfig::Mcp(domain::McpConnection {
                endpoint: Some(domain::ConnectionEndpoint {
                    adapter: "mcp".into(),
                    endpoint: String::new(),
                    options_schema: String::new(),
                    options: "{}".into(),
                }),
                allowed_tools: vec!["read".into()],
                command: "npx".into(),
                args: vec!["-y".into(), "pkg".into()],
                cwd: "/tmp".into(),
                env,
                headers: Default::default(),
                auth_header: "X-Key".into(),
                secret_env_var: "API_KEY".into(),
            })),
        };
        let wire_input: w::ConnectionInput = domain_input.clone().into();
        let wire_conn = w::Connection {
            id: wire_input.id,
            name: wire_input.name,
            enabled: wire_input.enabled,
            revision: wire_input.expected_revision,
            kind: wire_input.kind,
            config: wire_input.config,
            ..Default::default()
        };
        let round_trip: domain::Connection = wire_conn.into();
        assert_eq!(round_trip.to_input(), domain_input);

        let rules_update = domain::PolicyRulesUpdate {
            add_or_update: vec![domain::PolicyRule {
                id: "r1".into(),
                tool_name: "*".into(),
                pattern: "*".into(),
                effect: domain::PolicyEffect::AskLlm,
                created_at: "2026-01-01".into(),
            }],
            remove_ids: vec!["old".into()],
        };
        let wire_rules_req: w::UpdatePolicyRulesRequest = rules_update.clone().into();
        assert_eq!(wire_rules_req.remove_ids, rules_update.remove_ids);
        let back_rule: domain::PolicyRule = wire_rules_req.add_or_update[0].clone().into();
        assert_eq!(back_rule, rules_update.add_or_update[0]);
    }
}
