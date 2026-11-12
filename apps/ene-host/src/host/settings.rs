//! Provider connections, settings revision and the policy/approval surface.
use super::{
    db::{Result, Store, new_id, next_revision, now, validate_id},
    error::HostError,
    model,
    notify::Notifier,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialState {
    Missing,
    Registered,
}

impl CredentialState {
    pub(crate) fn parse(value: &str) -> Self {
        if value == "registered" {
            Self::Registered
        } else {
            Self::Missing
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Registered => "registered",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Connection {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    pub(crate) options: Value,
    pub(crate) credential_ref: Option<String>,
    pub(crate) credential_state: CredentialState,
    pub(crate) last_check: Value,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) revision: u64,
    pub(crate) connections: Vec<Connection>,
    pub(crate) updated_at: String,
}

pub(crate) fn settings(store: &Store) -> Result<w::Settings> {
    let settings = store.settings()?;
    Ok(w::Settings {
        revision: settings.revision,
        connections: settings.connections.iter().map(wire::connection).collect(),
        updated_at: settings.updated_at,
        ..Default::default()
    })
}

pub(crate) fn update_settings(
    world: &mut World,
    request: w::UpdateSettingsRequest,
) -> Result<(w::Settings, Vec<String>)> {
    let store = world.resource::<Store>().clone();
    let current = store.settings()?;
    if current.revision != request.expected_revision {
        return Err(HostError::conflict("settings revision changed"));
    }
    let mut existing = current.connections.clone();
    let mut next = Vec::new();
    for input in &request.connections {
        let id = if input.id.is_empty() {
            new_id()
        } else {
            input.id.clone()
        };
        validate_id(&id)?;
        if input.adapter.is_empty() {
            return Err(HostError::invalid("adapter is required"));
        }
        let endpoint = input.endpoint.trim().trim_end_matches('/').to_owned();
        let options = wire::parse(&input.options)?;
        super::model::validate_connection(&input.adapter, &input.kind, &endpoint, &options)?;
        let found = existing
            .iter()
            .position(|connection| connection.id == id)
            .map(|index| existing.remove(index));
        let connection = match found {
            Some(mut found) => {
                found.adapter = input.adapter.clone();
                found.kind = input.kind.clone();
                found.endpoint = endpoint;
                found.options = options;
                found.enabled = input.enabled;
                found
            }
            None => Connection {
                id,
                kind: input.kind.clone(),
                adapter: input.adapter.clone(),
                endpoint,
                options,
                credential_ref: None,
                credential_state: CredentialState::Missing,
                last_check: json!({}),
                enabled: input.enabled,
            },
        };
        next.push(connection);
    }
    let removed: Vec<String> = existing
        .into_iter()
        .map(|connection| connection.id)
        .collect();
    let forgotten = current
        .connections
        .iter()
        .filter(|connection| removed.contains(&connection.id))
        .filter_map(|connection| connection.credential_ref.clone())
        .collect();
    let revision = next_revision(current.revision)?;
    store.replace_connections(revision, &now(), &next, &removed)?;
    let view = settings(&store)?;
    notify_settings(world, &view);
    Ok((view, forgotten))
}

pub(crate) fn policy_rules(store: &Store) -> Result<w::PolicyRules> {
    Ok(w::PolicyRules {
        items: store.policy_rules()?,
        ..Default::default()
    })
}

pub(crate) fn update_policy_rules(
    world: &mut World,
    request: w::UpdatePolicyRulesRequest,
) -> Result<w::PolicyRules> {
    let store = world.resource::<Store>().clone();
    store.update_policy_rules(&request.add_or_update, &request.remove_ids)?;
    let rules = policy_rules(&store)?;
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
            rules: rules.items.clone(),
            ..Default::default()
        })),
    );
    Ok(rules)
}

pub(crate) fn pending_approvals(store: &Store) -> Result<w::PendingApprovals> {
    Ok(w::PendingApprovals {
        items: store.pending_approvals()?,
        ..Default::default()
    })
}

pub(crate) fn submit_approval(
    world: &mut World,
    request: w::SubmitApprovalRequest,
) -> Result<w::SubmitApprovalResponse> {
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval(&request.request_id)?
        .ok_or_else(|| HostError::not_found("approval request not found"))?;
    if approval.status != "pending" {
        return Err(HostError::conflict("approval is already resolved"));
    }
    if !world
        .resource::<super::resources::ApprovalHolds>()
        .0
        .contains_key(&request.request_id)
    {
        return Err(HostError::conflict(
            "approval is not attached to a live tool call",
        ));
    }
    let status = match request.decision.as_str() {
        "approve" => "approved",
        "always_approve" => "approved",
        "deny" => "denied",
        _ => return Err(HostError::invalid("invalid approval decision")),
    };
    if request.decision == "always_approve" && !confirm_matches(&store, &approval)? {
        let rule = w::PolicyRule {
            id: format!("approval-{}", approval.request_id),
            tool_name: approval.tool_name.clone(),
            pattern: approval.normalized_arguments.clone(),
            effect: "always_approve".into(),
            created_at: now(),
            ..Default::default()
        };
        store.update_policy_rules(&[rule], &[])?;
        notify_policy_rules(world, &store)?;
    }
    store.set_approval_status(&request.request_id, status)?;
    if let Some(hold) = world
        .resource_mut::<super::resources::ApprovalHolds>()
        .0
        .remove(&request.request_id)
        && world.get_entity(hold).is_ok()
    {
        world.despawn(hold);
    }
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "approval_resolved".into(),
            payload: json!({
                "request_id": request.request_id,
                "status": status,
                "reason": request.reason,
            })
            .to_string(),
            ..Default::default()
        })),
    );
    Ok(w::SubmitApprovalResponse {
        acknowledged: true,
        status: status.into(),
        ..Default::default()
    })
}

fn confirm_matches(store: &Store, approval: &w::ApprovalRequest) -> Result<bool> {
    Ok(store.policy_rules()?.into_iter().any(|rule| {
        rule.effect == "confirm"
            && (rule.tool_name.is_empty()
                || rule.tool_name == "*"
                || rule.tool_name == approval.tool_name)
            && (rule.pattern.is_empty() || approval.normalized_arguments.contains(&rule.pattern))
    }))
}

fn notify_policy_rules(world: &mut World, store: &Store) -> Result<()> {
    let rules = policy_rules(store)?;
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
            rules: rules.items.clone(),
            ..Default::default()
        })),
    );
    Ok(())
}

fn notify_settings(world: &mut World, view: &w::Settings) {
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
            settings: view.clone().into(),
            ..Default::default()
        })),
    );
}

pub(crate) fn credential_changed(world: &mut World, connection_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store.bump_settings()?;
    model::invalidate_connection(world, connection_id);
    let view = settings(&store)?;
    notify_settings(world, &view);
    Ok(())
}

pub(crate) fn expire_orphan_holds(world: &mut World) {
    let stale: Vec<(String, bevy_ecs::prelude::Entity)> = world
        .resource::<super::resources::ApprovalHolds>()
        .0
        .iter()
        .filter(|(_, hold)| world.get_entity(**hold).is_err())
        .map(|(request, hold)| (request.clone(), *hold))
        .collect();
    if stale.is_empty() {
        return;
    }
    let store = world.resource::<Store>().clone();
    for (request, _) in stale {
        if let Err(error) = store.set_approval_status(&request, "expired") {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            return;
        }
        world
            .resource_mut::<super::resources::ApprovalHolds>()
            .0
            .remove(&request);
    }
}
