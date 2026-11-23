//! The domain of a declared client: capabilities, grants, the active client
//! and device operations.

use super::error::{HostError, Result};
use super::json;

pub(crate) use ene_domain::{Client, DeviceOperation, DeviceStatus};
#[cfg(test)]
pub(crate) use ene_policy::{BUILTIN_TOOLS, approval_target};
pub(crate) use ene_policy::{
    CapabilityApproval, CapabilitySpec, WORKSPACE_CAPABILITY_PREFIX, validate_declarations,
};

/// Validates `input` against the declared schema of `spec`.
pub(crate) fn validate_input(spec: &CapabilitySpec, input: &serde_json::Value) -> Result<()> {
    let schema = json::parse_schema(&spec.input_schema)?;
    json::validate(&schema, input).map_err(|error| HostError::Invalid(error.to_string()))
}

/// Validates a client's operation output against its declared output schema.
pub(crate) fn validate_output(spec: &CapabilitySpec, output: &serde_json::Value) -> Result<()> {
    if spec.output_schema.trim().is_empty() {
        return Ok(());
    }
    let schema = json::parse_schema(&spec.output_schema)?;
    json::validate(&schema, output).map_err(|error| HostError::Invalid(error.to_string()))
}

// Clients: capability declarations, the active client and perception.
//
// The host stores capability names, JSON Schema, presets and approval
// declarations only. A declared capability with a `tool_name` becomes a tool
// for the parent while it is granted and the client is active; every
// operation is still gated by `permission::gate` and dispatched to the client
// as a `DeviceOperation`.
pub(crate) mod computer;
pub(crate) mod device;
pub(crate) mod dynamic;

use super::{
    db::Store,
    notify::Notifier,
    resources::{DeviceHub, Perception, PerceptionEvent},
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::Value;

/// The environment variable that decides which declared capabilities the host
/// grants: a comma list of names, or `*` for every declaration. It is read
/// once when the app is built; an unset or empty value grants nothing.
pub(crate) const ALLOWED_CAPABILITIES_ENV: &str = "ENE_HOST_ALLOWED_CAPABILITIES";

/// What the host is willing to grant a client. A declaration alone grants
/// nothing: the policy is the host-side decision, and the operation-level
/// gate still decides every call.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityPolicy {
    pub(crate) allowed: Vec<String>,
    /// `*` in the environment: grant whatever a client declares.
    pub(crate) all: bool,
}

impl CapabilityPolicy {
    /// Nothing is granted: the default when the environment names no policy.
    pub(crate) fn none() -> Self {
        Self {
            allowed: Vec::new(),
            all: false,
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut policy = Self::none();
        for capability in text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if capability == "*" {
                policy.all = true;
            } else if !policy.allowed.iter().any(|allowed| allowed == capability) {
                policy.allowed.push(capability.to_owned());
            }
        }
        policy
    }

    pub(crate) fn allows(&self, capability: &str) -> bool {
        self.all
            || self.allowed.iter().any(|allowed| {
                allowed == capability
                    || allowed
                        .strip_suffix('*')
                        .is_some_and(|prefix| !prefix.is_empty() && capability.starts_with(prefix))
            })
    }

    /// Dynamically allows a capability pattern (`exact` or `prefix.*`) at runtime (`F11`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn allow_pattern(&mut self, pattern: impl Into<String>) {
        let pattern = pattern.into();
        let trimmed = pattern.trim();
        if trimmed == "*" {
            self.all = true;
        } else if !trimmed.is_empty() && !self.allowed.iter().any(|p| p == trimmed) {
            self.allowed.push(trimmed.to_owned());
        }
    }

    /// Dynamically revokes a capability pattern at runtime (`F11`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn revoke_pattern(&mut self, pattern: &str) {
        let trimmed = pattern.trim();
        if trimmed == "*" {
            self.all = false;
        } else {
            self.allowed.retain(|p| p != trimmed);
        }
    }

    /// The declared capabilities this policy grants, in declaration order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn grants(&self, declared: &[String]) -> Vec<String> {
        declared
            .iter()
            .filter(|capability| self.allows(capability))
            .cloned()
            .collect()
    }

    /// Grants declared capabilities allowed by either the static env policy or
    /// saved `PolicyRule`s targeting `client_capability` (`F11`).
    pub(crate) fn grants_with_rules(
        &self,
        declared: &[String],
        rules: &[super::permission::PolicyRule],
    ) -> Vec<String> {
        use super::permission::PolicyEffect;
        declared
            .iter()
            .filter(|capability| {
                let mut denied = false;
                let mut rule_allowed = false;
                for rule in rules
                    .iter()
                    .filter(|r| r.tool_name == "client_capability" || r.tool_name == "*")
                {
                    let matches = rule.pattern == "*"
                        || rule.pattern == **capability
                        || rule.pattern.strip_suffix('*').is_some_and(|prefix| {
                            !prefix.is_empty() && capability.starts_with(prefix)
                        });
                    if !matches {
                        continue;
                    }
                    match rule.effect {
                        PolicyEffect::Deny => {
                            denied = true;
                            break;
                        }
                        PolicyEffect::Allow => rule_allowed = true,
                        PolicyEffect::Confirm | PolicyEffect::AskLlm => {}
                    }
                }
                !denied && (self.allows(capability) || rule_allowed)
            })
            .cloned()
            .collect()
    }
}

impl FromWorld for CapabilityPolicy {
    fn from_world(_world: &mut World) -> Self {
        std::env::var(ALLOWED_CAPABILITIES_ENV)
            .map_or_else(|_| Self::none(), |text| Self::parse(&text))
    }
}

pub(crate) use dynamic::CapabilityTools;

#[derive(Clone, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeviceRequestArgs {
    /// The capability name the client declared.
    pub(crate) capability: String,
    /// The capability input, validated against the declared JSON Schema.
    #[serde(default)]
    pub(crate) input: Value,
}

/// Registers the generic dispatcher, the dynamic capability tools and their
/// systems. A capability tool is spawned when a client grants it and despawned
/// when the declaration or the activation changes.
pub(crate) struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        let wake = app
            .world()
            .get_resource::<rig_ecs::tasks::Wake>()
            .cloned()
            .unwrap_or_default();
        app.add_open_tool::<DeviceRequestArgs, _>(
            "device_request",
            "Run a listed capability on the bound client. Use its declared JSON input schema. \
             The tool waits for the client's validated result.",
            ToolOptions::default(),
            device::on_device_request,
        )
        .add_systems(
            AgentPass,
            device::describe_device_request.in_set(PrepareSystems::Edit),
        )
        .insert_resource(dynamic::CapabilityCalls::new(wake))
        .init_resource::<CapabilityPolicy>()
        .init_resource::<CapabilityTools>()
        .init_resource::<computer::ComputerUse>()
        .init_resource::<super::eyes::Eyes>()
        .add_systems(
            Update,
            (
                dynamic::drain.after(AgentLoop),
                super::eyes::poll.after(AgentLoop),
            ),
        );
    }
}

/// Every registered client, with its live connection state.
pub(crate) fn clients(store: &Store, hub: &DeviceHub) -> Result<Vec<w::Client>> {
    let activations = store.client_repo().activations()?;
    Ok(store
        .client_repo()
        .clients()?
        .into_iter()
        .map(|client| {
            let active: Vec<String> = activations
                .iter()
                .filter(|activation| activation.client_id.as_deref() == Some(client.id.as_str()))
                .map(|activation| activation.companion_id.clone())
                .collect();
            wire::client(&client, hub.connected(&client.id), active)
        })
        .collect())
}

/// Converts and validates one declaration set.
fn declarations(
    specs: &[w::ClientCapabilitySpec],
    capabilities: &[String],
) -> Result<Vec<CapabilitySpec>> {
    let specs: Vec<CapabilitySpec> = specs.iter().map(wire::capability_spec_from).collect();
    super::client::validate_declarations(capabilities, &specs)?;
    if capabilities.iter().any(|name| name == w::COMPUTER_USE)
        && !specs
            .iter()
            .any(|spec| spec.name == w::COMPUTER_USE && spec.tool_name.is_empty())
    {
        return Err(HostError::Invalid(
            ("computer.use requires a schema and is only available through device_request").into(),
        ));
    }
    Ok(specs)
}

/// Publishes the client view, refreshes the capability tools and returns it.
fn publish(world: &mut World, client: &Client) -> Result<w::Client> {
    let connected = world.resource::<DeviceHub>().connected(&client.id);
    let active = world
        .resource::<Store>()
        .client_repo()
        .active_companions_for_client(&client.id)?;
    let view = wire::client(client, connected, active);
    dynamic::sync_tools(world)?;
    if !client.is_granted(w::COMPUTER_USE) {
        computer::invalidate_client(world, &client.id, None);
    }
    notify_client(world, &view);
    Ok(view)
}

/// Registers or refreshes a client declaration. A client never sends grants:
/// the host's [`CapabilityPolicy`] decides them, and the operation-level gate
/// still decides every call.
pub(crate) fn register(world: &mut World, request: w::RegisterClientRequest) -> Result<w::Client> {
    let store = world.resource::<Store>().clone();
    let specs = declarations(&request.capability_specs, &request.capabilities)?;
    let rules = store.policy_repo().policy_rules()?;
    let granted = world
        .resource::<CapabilityPolicy>()
        .grants_with_rules(&request.capabilities, &rules);
    let previous = store.client_repo().client(&request.id)?;
    let client = store.client_repo().register_client_with_grants(
        &super::db::clients::ClientDeclaration {
            id: &request.id,
            name: &request.name,
            platform: &request.platform,
            capabilities: &request.capabilities,
            specs: &specs,
            expected_revision: request.expected_revision,
        },
        &granted,
    )?;
    match publish(world, &client) {
        Ok(view) => Ok(view),
        Err(error) => {
            if let Some(previous) = previous {
                let _ = store.client_repo().restore_client(&previous);
            } else {
                let _ = store.client_repo().delete_client(&request.id);
            }
            let _ = dynamic::sync_tools(world);
            Err(error)
        }
    }
}

pub(crate) fn set_capabilities(
    world: &mut World,
    request: w::SetCapabilitiesRequest,
) -> Result<w::Client> {
    let store = world.resource::<Store>().clone();
    let specs = declarations(&request.capability_specs, &request.capabilities)?;
    if !request.granted_capabilities.is_empty() {
        return Err(HostError::Permission(
            ("a client cannot grant capabilities to itself").into(),
        ));
    }
    let rules = store.policy_repo().policy_rules()?;
    let granted = world
        .resource::<CapabilityPolicy>()
        .grants_with_rules(&request.capabilities, &rules);
    let previous = store.client_repo().client(&request.client_id)?;
    let client = store.client_repo().set_capabilities(
        &request.client_id,
        &request.capabilities,
        &granted,
        &specs,
        request.revision,
    )?;
    match publish(world, &client) {
        Ok(view) => Ok(view),
        Err(error) => {
            if let Some(previous) = previous {
                let _ = store.client_repo().restore_client(&previous);
            }
            let _ = dynamic::sync_tools(world);
            Err(error)
        }
    }
}

fn notify_client(world: &mut World, client: &w::Client) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        None,
        Some(&client.id),
        w::notification::Event::ClientUpdated(Box::new(w::ClientUpdated {
            client: Some(client.clone()).into(),
            ..Default::default()
        })),
    );
}

/// Moves or releases the active client. The latest write wins: the moved-to
/// client is the active one, and work already accepted by the old client keeps
/// running there. Its audio output is flushed, its perception summary is
/// dropped and its tools are unpublished.
pub(crate) fn activate(world: &mut World, request: w::ActivationRequest) -> Result<w::Activation> {
    let store = world.resource::<Store>().clone();
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    if !request.release {
        super::util::validate_id(&request.client_id)?;
        store
            .client_repo()
            .client(&request.client_id)?
            .ok_or_else(|| HostError::NotFound(("client not found").into()))?;
    }
    let previous = store.client_repo().activation(&request.companion_id)?;
    let client = (!request.release).then_some(request.client_id.as_str());
    let activation = store
        .client_repo()
        .activate(&request.companion_id, client)?;
    // Publish the new active client first: a conflicting tool declaration must
    // roll the move back before any notification is visible.
    if let Err(error) = dynamic::sync_tools(world) {
        if let Some(previous) = previous.as_ref() {
            let _ = store.client_repo().restore_activation(previous);
        } else {
            let _ = store.client_repo().delete_activation(&request.companion_id);
        }
        let _ = dynamic::sync_tools(world);
        return Err(error);
    }
    if let Some(previous) = previous.clone()
        && let Some(old_client) = previous.client_id
        && client != Some(old_client.as_str())
    {
        super::voice::on_client_moved(
            world.get_resource::<super::voice::Voice>(),
            &request.companion_id,
            &old_client,
        );
        world
            .resource_mut::<Perception>()
            .events
            .remove(&request.companion_id);
        super::eyes::clear(
            &mut world.resource_mut::<super::eyes::Eyes>(),
            &request.companion_id,
        );
        // The old client keeps its stream but is no longer the active one.
        if let Some(old) = store.client_repo().client(&old_client)? {
            let hub = world.resource::<DeviceHub>();
            let active = store.client_repo().active_companions_for_client(&old.id)?;
            notify_client(world, &wire::client(&old, hub.connected(&old.id), active));
        }
    }
    if !request.release {
        super::voice::on_client_activated(
            world.get_resource::<super::voice::Voice>(),
            &request.companion_id,
            &request.client_id,
        );
    }
    world.resource_mut::<Notifier>().push(
        Some(&request.companion_id),
        None,
        w::notification::Event::ActivationUpdated(Box::new(wire::activation(&activation))),
    );
    Ok(wire::activation(&activation))
}

/// Routes live observations to the eyes and supports legacy passive summaries.
pub(crate) fn report_perception(world: &mut World, event: w::PerceptionEvent) -> Result<()> {
    let store = world.resource::<Store>().clone();
    super::util::validate_id(&event.client_id)?;
    let client = store
        .client_repo()
        .client(&event.client_id)?
        .ok_or_else(|| HostError::NotFound(("client not found").into()))?;
    let active_companions = store
        .client_repo()
        .active_companions_for_client(&event.client_id)?;
    if active_companions.is_empty() {
        // A connected client that is not active produces nothing the head may
        // act on. Not an error: the event is simply discarded.
        return Ok(());
    }
    let targets: Vec<String> = match event.companion_id.as_deref().filter(|id| !id.is_empty()) {
        Some(target) => {
            if !active_companions.iter().any(|id| id == target) {
                return Ok(());
            }
            vec![target.to_owned()]
        }
        None => active_companions,
    };
    if !event.subscription_id.is_empty() {
        if matches!(
            &event.event,
            Some(w::perception_event::Event::WatchMatch(_))
        ) && !client.is_granted("eye.watch")
        {
            return Ok(());
        }
        for companion_id in targets {
            super::eyes::report_with(world, companion_id, event.clone())?;
        }
        return Ok(());
    }
    let Some(summary) = perception_summary(&event) else {
        return Ok(());
    };
    let avatar_interaction = match &event.event {
        Some(w::perception_event::Event::AvatarInteraction(interaction)) => Some((
            format!("{:?}", interaction.kind),
            interaction.character_id.clone(),
            interaction.region.clone(),
        )),
        _ => None,
    };
    {
        let mut perception = world.resource_mut::<Perception>();
        for companion_id in &targets {
            perception.events.insert(
                companion_id.clone(),
                PerceptionEvent {
                    summary: summary.clone(),
                },
            );
        }
    }
    if let Some((kind, character_id, region)) = avatar_interaction {
        for companion_id in targets {
            world.trigger(super::resources::TriggerCharacterEvent {
                companion_id,
                event: ene_plugin::CharacterEvent::Custom {
                    name: "avatar_interaction".into(),
                    data: serde_json::json!({
                        "kind": kind,
                        "character_id": character_id,
                        "region": region,
                    }),
                },
            });
        }
    }
    Ok(())
}

fn perception_summary(event: &w::PerceptionEvent) -> Option<String> {
    let summary = match event.event.as_ref()? {
        w::perception_event::Event::ActiveWindow(active) => {
            if active.title.is_empty() && active.app_id.is_empty() {
                return None;
            }
            format!("アクティブウィンドウ: {} ({})", active.title, active.app_id)
        }
        w::perception_event::Event::UserActivity(state) => {
            if state.is_active {
                "ユーザーは操作中です".to_owned()
            } else {
                format!("ユーザーは離席中です (idle {} 秒)", state.idle_seconds)
            }
        }
        w::perception_event::Event::PerceptionSummary(summary) => {
            if summary.summary.trim().is_empty() {
                return None;
            }
            summary.summary.clone()
        }
        w::perception_event::Event::AvatarInteraction(interaction) => {
            let kind_label = match interaction.kind.as_known() {
                Some(w::AvatarInteractionKind::DoubleClick) => "ダブルクリック",
                Some(w::AvatarInteractionKind::Drag) => "ドラッグ",
                _ => "クリック",
            };
            if interaction.region.trim().is_empty() {
                format!("ユーザーがアバターを{kind_label}しました")
            } else {
                format!(
                    "ユーザーがアバター({})を{kind_label}しました",
                    interaction.region.trim()
                )
            }
        }
        _ => return None,
    };
    Some(super::util::truncate(&summary, 1_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn motion() -> CapabilitySpec {
        CapabilitySpec {
            name: "body.motion".into(),
            tool_name: "body_motion".into(),
            approval: CapabilityApproval::Auto,
            ..Default::default()
        }
    }

    #[test]
    fn approval_targets_expand_verified_input_and_preserve_unclosed_fields() {
        let spec = CapabilitySpec {
            approval_target: "write {path} as {mode}".into(),
            approval: CapabilityApproval::Confirm,
            ..motion()
        };
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a", "mode": "text"})),
            "write /tmp/a as text"
        );
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a"})),
            "write /tmp/a as "
        );
        let spec = CapabilitySpec {
            approval_target: "{path}".into(),
            ..motion()
        };
        assert_eq!(approval_target(&spec, &json!({"path": "/tmp/a"})), "/tmp/a");
        let spec = CapabilitySpec {
            approval_target: "write {path} as {mode".into(),
            ..motion()
        };
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a", "mode": "text"})),
            "write /tmp/a as {mode"
        );
    }

    #[test]
    fn a_spec_must_name_a_declared_capability_and_cannot_shadow_a_host_tool() {
        assert!(validate_declarations(&["body.motion".into()], &[motion()]).is_ok());
        let shadow = CapabilitySpec {
            tool_name: "mind_recall".into(),
            ..motion()
        };
        assert!(validate_declarations(&["body.motion".into()], &[shadow]).is_err());
    }

    #[test]
    fn f11_dynamic_capability_policy_prefix_and_rules() {
        let mut policy = CapabilityPolicy::parse("body.motion");
        let declared = vec![
            "body.motion".to_owned(),
            "plugin.echo.run".to_owned(),
            "plugin.other.run".to_owned(),
        ];
        assert_eq!(policy.grants(&declared), vec!["body.motion"]);
        policy.allow_pattern("plugin.echo.*");
        assert_eq!(
            policy.grants(&declared),
            vec!["body.motion", "plugin.echo.run"]
        );
        policy.revoke_pattern("plugin.echo.*");
        assert_eq!(policy.grants(&declared), vec!["body.motion"]);
        let rules = vec![
            super::super::permission::PolicyRule {
                id: "allow-plugins".into(),
                tool_name: "client_capability".into(),
                pattern: "plugin.*".into(),
                effect: super::super::permission::PolicyEffect::Allow,
                created_at: "2026-11-20T00:00:00Z".into(),
            },
            super::super::permission::PolicyRule {
                id: "deny-other".into(),
                tool_name: "client_capability".into(),
                pattern: "plugin.other.*".into(),
                effect: super::super::permission::PolicyEffect::Deny,
                created_at: "2026-11-20T00:00:00Z".into(),
            },
        ];
        assert_eq!(
            policy.grants_with_rules(&declared, &rules),
            vec!["body.motion", "plugin.echo.run"]
        );
    }
}
