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

impl From<domain::HostHealth> for w::HostHealth {
    fn from(value: domain::HostHealth) -> Self {
        Self {
            ready: value == domain::HostHealth::Ready,
            recovery_required: value == domain::HostHealth::RecoveryRequired,
            stopping: value == domain::HostHealth::Stopping,
            ..Default::default()
        }
    }
}

from_fields! {
    w::Client => domain::ClientInfo {
        id, capabilities, granted_capabilities, revision,
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

impl From<w::Snapshot> for domain::Snapshot {
    fn from(value: w::Snapshot) -> Self {
        Self {
            settings: value.settings.into_option().map(Into::into),
            companions: value.companions.into_iter().map(Into::into).collect(),
            active_work: value
                .work_sessions
                .into_iter()
                .filter_map(|session| {
                    if session.id.is_empty()
                        || session.status.as_known() != Some(w::WorkStatus::Active)
                    {
                        return None;
                    }
                    Some((
                        session.id,
                        ene_domain::CompanionId::parse(&session.companion_id).ok()?,
                    ))
                })
                .collect(),
            active_companion_id: value.active_companion_id,
            pending_approvals: value
                .pending_approvals
                .into_iter()
                .map(Into::into)
                .collect(),
            clients: value.clients.into_iter().map(Into::into).collect(),
            policy_rules: policy_rules(value.policy_rules),
            activations: value
                .activations
                .into_iter()
                .filter_map(|item| item.try_into().ok())
                .collect(),
        }
    }
}

from_fields! {
    w::HostEvent => domain::HostEvent {
        kind,
        payload.into_option().map(Into::into),
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
            cursor: host_cursor(value.cursor.into_option()),
            event: value
                .event
                .map(|event| notification_event(event, value.companion_id.as_deref())),
        }
    }
}

impl From<w::Snapshot> for domain::HostNotification {
    fn from(mut value: w::Snapshot) -> Self {
        let cursor = host_cursor(std::mem::take(&mut value.cursor).into_option());
        Self::initial_state(value.into(), cursor)
    }
}

/// Converts a usable host notification position to the client model.
pub fn host_cursor(cursor: Option<w::Cursor>) -> Option<domain::HostCursor> {
    cursor
        .filter(|item| !item.epoch.is_empty())
        .map(|item| domain::HostCursor {
            epoch: item.epoch,
            seq: item.seq,
        })
}

/// Discards rules whose effect is outside the current contract.
pub fn policy_rules(rules: Vec<w::PolicyRule>) -> Vec<domain::PolicyRule> {
    rules
        .into_iter()
        .filter_map(|rule| rule.try_into().ok())
        .collect()
}

fn notification_event(
    wire: w::notification::Event,
    companion_id: Option<&str>,
) -> domain::NotificationEvent {
    use domain::NotificationEvent as Event;
    use w::notification::Event as Wire;

    match wire {
        Wire::Health(_) => Event::Other,
        Wire::Companion(value) => Event::Companion(Box::new((*value).into())),
        Wire::CompanionRemoved(value) => {
            ene_domain::CompanionId::parse(&value.id).map_or(Event::Other, Event::CompanionRemoved)
        }
        Wire::MindUpdated(value) => ene_domain::CompanionId::parse(&value.companion_id)
            .map_or(Event::Other, Event::MindUpdated),
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
        Wire::WorkEvent(value) => work_status(*value, companion_id).unwrap_or(Event::Other),
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

fn work_status(
    work: w::WorkEvent,
    companion_id: Option<&str>,
) -> Option<domain::NotificationEvent> {
    let w::work_event::Detail::StatusChanged(change) = work.detail? else {
        return None;
    };
    if work.work_id.is_empty() {
        return None;
    }
    Some(domain::NotificationEvent::WorkStatusChanged {
        work_id: work.work_id,
        companion_id: ene_domain::CompanionId::parse(companion_id?).ok()?,
        status: change.status.as_known()?.try_into().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_activity_is_projected_from_session_status() {
        let companion = ene_domain::CompanionId::parse("ene").unwrap();
        let snapshot: domain::Snapshot = w::Snapshot {
            work_sessions: [
                ("active", w::WorkStatus::Active),
                ("idle", w::WorkStatus::Idle),
            ]
            .into_iter()
            .map(|(id, status)| w::WorkSession {
                id: id.into(),
                companion_id: "ene".into(),
                status: status.into(),
                ..Default::default()
            })
            .collect(),
            ..Default::default()
        }
        .into();
        assert_eq!(
            snapshot.active_work,
            [("active".into(), companion.clone())].into()
        );
        let notify = |detail| {
            domain::HostNotification::from(w::Notification {
                companion_id: Some("ene".into()),
                event: Some(w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
                    work_id: "active".into(),
                    run_id: "previous-run".into(),
                    detail: Some(detail),
                    ..Default::default()
                }))),
                ..Default::default()
            })
            .event
        };
        assert_eq!(
            notify(w::work_event::Detail::RunFinished(Box::default())),
            Some(domain::NotificationEvent::Other)
        );
        assert_eq!(
            notify(w::work_event::Detail::StatusChanged(Box::new(
                w::WorkStatusChanged {
                    status: w::WorkStatus::Idle.into(),
                    ..Default::default()
                }
            ))),
            Some(domain::NotificationEvent::WorkStatusChanged {
                work_id: "active".into(),
                companion_id: companion,
                status: domain::WorkStatus::Idle,
            })
        );
    }

    #[test]
    fn wire_acl_preserves_connection_template_and_policy_round_trips() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("FOO".to_owned(), "bar".to_owned());
        let domain_input = domain::ConnectionInput {
            id: "mcp-1".into(),
            name: "MCP".into(),
            enabled: true,
            expected_revision: 4,
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
        assert_eq!(wire_input.kind.as_known(), Some(w::ConnectionKind::Mcp));
        let wire_conn = w::Connection {
            id: wire_input.id,
            name: wire_input.name,
            enabled: wire_input.enabled,
            revision: wire_input.expected_revision,
            kind: wire_input.kind,
            config: wire_input.config,
            credential_state: "environment".into(),
            ..Default::default()
        };
        let mut round_trip: domain::Connection = wire_conn.into();
        assert_eq!(round_trip.to_input(), domain_input);
        assert_eq!(
            round_trip.credential_state,
            ene_domain::CredentialState::Environment
        );
        round_trip.kind = domain::ConnectionKind::Llm;
        assert!(round_trip.to_input().config.is_none());

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
