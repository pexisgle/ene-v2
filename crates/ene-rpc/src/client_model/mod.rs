//! Anti-corruption layer (ACL) conversions between `ene_rpc` wire types and pure domain models.

// Each mapping lists matching fields and the transformations at the wire boundary.
macro_rules! from_fields {
    (
        $source:ty => $target:ty {
            $($field:ident $(.$method:ident($($arg:expr),*))*,)+
            $(..$default:expr)?
        }
    ) => {
        impl From<$source> for $target {
            fn from(input: $source) -> Self {
                Self {
                    $($field: input.$field $(.$method($($arg),*))* ,)+
                    $(..$default)?
                }
            }
        }
    };
}

macro_rules! from_variants {
    ($source:ty => $target:ty [$($variant:ident),+ $(,)?]) => {
        impl From<$source> for $target {
            fn from(input: $source) -> Self {
                match input {
                    $(<$source>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

pub mod companion;
pub mod connection;
pub mod policy;

use crate as w;
use ene_domain::client_view as domain;

from_fields! {
    w::JsonDocument => domain::JsonDocument {
        schema, json,
    }
}

from_fields! {
    domain::JsonDocument => w::JsonDocument {
        schema, json,
        ..Default::default()
    }
}

from_fields! {
    w::HostHealth => domain::HostHealth {
        ready, recovery_required, stopping,
    }
}

from_fields! {
    w::Client => domain::ClientInfo {
        id, name, platform, capabilities, granted_capabilities, revision,
        last_seen, connected, active_companions,
    }
}

impl From<w::WorkEvent> for domain::WorkEvent {
    fn from(value: w::WorkEvent) -> Self {
        let detail = value.detail.and_then(|detail| match detail {
            w::work_event::Detail::StatusChanged(changed) => changed
                .status
                .as_known()
                .and_then(|status| status.try_into().ok())
                .map(domain::WorkEventDetail::StatusChanged),
            w::work_event::Detail::RunStarted(_) => Some(domain::WorkEventDetail::RunStarted),
            w::work_event::Detail::RunFinished(finished) => {
                Some(domain::WorkEventDetail::RunFinished {
                    state: finished
                        .state
                        .as_known()
                        .and_then(|state| state.try_into().ok()),
                    failure: finished.failure.into_option().map(Into::into),
                    pending_request_id: finished.pending_request_id,
                })
            }
        });
        Self {
            work_id: value.work_id,
            run_id: value.run_id,
            detail,
        }
    }
}

from_fields! {
    w::Settings => domain::Settings {
        revision,
        connections.into_iter().map(Into::into).collect(),
        updated_at,
        policy_texts.into_iter().map(Into::into).collect(),
    }
}

from_fields! {
    w::Snapshot => domain::Snapshot {
        server_version,
        health.into_option().map(Into::into),
        settings.into_option().map(Into::into),
        companions.into_iter().map(Into::into).collect(),
        work_sessions.into_iter().filter_map(|item| item.try_into().ok()).collect(),
        active_companion_id,
        pending_approvals.into_iter().map(Into::into).collect(),
        clients.into_iter().map(Into::into).collect(),
        policy_rules.into_iter().filter_map(|rule| rule.try_into().ok()).collect(),
        activations.into_iter().filter_map(|value| value.try_into().ok()).collect(),
    }
}

from_fields! {
    w::HostEvent => domain::HostEvent {
        kind,
        payload.into_option().map(Into::into),
    }
}

from_fields! {
    w::MindUpdated => domain::MindUpdated {
        companion_id, profile_revision, state_revision, updated_at,
    }
}

from_fields! {
    w::MessageDelta => domain::MessageDelta {
        companion_id, stream_id, delta,
    }
}

impl From<w::Notification> for domain::HostNotification {
    fn from(value: w::Notification) -> Self {
        Self {
            kind: value.kind,
            event: value.event.map(notification_event),
        }
    }
}

/// Discards rules whose effect is outside the current contract.
pub fn policy_rules(rules: Vec<w::PolicyRule>) -> Vec<domain::PolicyRule> {
    rules
        .into_iter()
        .filter_map(|rule| rule.try_into().ok())
        .collect()
}

fn notification_event(wire: w::notification::Event) -> domain::NotificationEvent {
    use domain::NotificationEvent as Event;
    use w::notification::Event as Wire;

    match wire {
        Wire::Health(value) => Event::Health((*value).into()),
        Wire::Companion(value) => Event::Companion(Box::new((*value).into())),
        Wire::MindUpdated(value) => Event::MindUpdated((*value).into()),
        Wire::MessageCreated(value) => value
            .message
            .into_option()
            .and_then(|message| domain::Message::try_from(message).ok())
            .map_or(Event::Other, |message| {
                Event::MessageCreated(Box::new(message))
            }),
        Wire::MessageDelta(value) => Event::MessageDelta((*value).into()),
        Wire::TurnEvent(value) => domain::TurnEvent::try_from(*value)
            .map_or(Event::Other, |turn| Event::TurnEvent(Box::new(turn))),
        Wire::WorkEvent(value) => Event::WorkEvent((*value).into()),
        Wire::ApprovalRequested(value) => Event::ApprovalRequested(
            value
                .request
                .into_option()
                .map(|item| Box::new(item.into())),
        ),
        Wire::PolicyRulesUpdated(value) => Event::PolicyRulesUpdated(policy_rules(value.rules)),
        Wire::PolicyTextsUpdated(value) => {
            Event::PolicyTextsUpdated(value.texts.into_iter().map(Into::into).collect())
        }
        Wire::SettingsUpdated(value) => Event::SettingsUpdated(
            value
                .settings
                .into_option()
                .map(|item| Box::new(item.into())),
        ),
        Wire::ClientUpdated(value) => {
            Event::ClientUpdated(value.client.into_option().map(|item| Box::new(item.into())))
        }
        Wire::Generic(value) => Event::Generic((*value).into()),
        Wire::InitialState(value) => Event::InitialState(Box::new((*value).into())),
        Wire::ResyncRequired(value) => Event::ResyncRequired {
            reason: value.reason,
        },
        Wire::ActivationUpdated(value) => (*value)
            .try_into()
            .map_or(Event::Other, Event::ActivationUpdated),
        Wire::DeviceOperation(_) | Wire::DeviceOperationResult(_) => Event::Other,
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
        let back_rule: domain::PolicyRule =
            wire_rules_req.add_or_update[0].clone().try_into().unwrap();
        assert_eq!(back_rule, rules_update.add_or_update[0]);
    }
}
