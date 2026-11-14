//! The domain of a declared client: capabilities, grants, activation
//! generations and device operations.

use super::error::{Failure, HostError, Result};
use super::json;
use serde::{Deserialize, Serialize};

/// Tools the host itself owns. A client capability may not shadow one.
pub(crate) const BUILTIN_TOOLS: &[&str] = &[
    "mind_recall",
    "mind_read",
    "mind_update",
    "start_work",
    "send_work",
    "list_works",
    "stop_work",
    "device_request",
    "noop",
    "schedule_create",
    "schedule_list",
    "schedule_cancel",
    "report",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityApproval {
    /// Follow the host's default policy; an unknown capability needs confirmation.
    #[default]
    Unspecified,
    /// Run when policy allows it.
    Auto,
    /// A Stage::Gate approval is required for every operation.
    Confirm,
}

/// A capability a client declares, with the JSON Schema the host validates
/// operations against.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CapabilitySpec {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) tool_name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default = "default_schema")]
    pub(crate) input_schema: String,
    #[serde(default)]
    pub(crate) presets: Vec<String>,
    #[serde(default)]
    pub(crate) approval: CapabilityApproval,
    #[serde(default)]
    pub(crate) approval_target: String,
    #[serde(default)]
    pub(crate) output_schema: String,
}

fn default_schema() -> String {
    r#"{"type":"object"}"#.into()
}

impl CapabilitySpec {
    pub(crate) fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.contains(char::is_whitespace) {
            return Err(HostError::invalid(
                "capability names must be 1-64 non-space characters",
            ));
        }
        if !self.tool_name.is_empty() {
            if self.tool_name.chars().count() > 64
                || self
                    .tool_name
                    .bytes()
                    .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
            {
                return Err(HostError::invalid(
                    "capability tool names must be 1-64 [A-Za-z0-9_-] characters",
                ));
            }
            if BUILTIN_TOOLS.contains(&self.tool_name.as_str()) {
                return Err(HostError::invalid(format!(
                    "tool name `{}` is reserved by the host",
                    self.tool_name
                )));
            }
        }
        if self.description.chars().count() > 2_000 {
            return Err(HostError::invalid("capability description is too long"));
        }
        if self.approval_target.chars().count() > 500 {
            return Err(HostError::invalid("capability approval target is too long"));
        }
        let schema = json::parse_schema(&self.input_schema)?;
        if !schema.is_object() {
            return Err(HostError::invalid(
                "capability input schema must be an object",
            ));
        }
        if !self.output_schema.trim().is_empty() {
            json::parse_schema(&self.output_schema)?;
        }
        if self.presets.len() > 64
            || self
                .presets
                .iter()
                .any(|preset| preset.chars().count() > 64)
        {
            return Err(HostError::invalid("capability presets are too long"));
        }
        Ok(())
    }

    /// The model-facing description, with the presets the client declared.
    pub(crate) fn tool_description(&self) -> String {
        let mut description = self.description.clone();
        if !self.presets.is_empty() {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str("Presets: ");
            description.push_str(&self.presets.join(", "));
            description.push('.');
        }
        description
    }
}

/// Validates a whole declaration set: every spec names a declared capability,
/// names are unique and tool names do not collide.
pub(crate) fn validate_declarations(
    capabilities: &[String],
    specs: &[CapabilitySpec],
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut tools = std::collections::HashSet::new();
    for capability in capabilities {
        if !seen.insert(capability.as_str()) {
            return Err(HostError::invalid(format!(
                "capability `{capability}` is declared twice"
            )));
        }
    }
    for spec in specs {
        if !capabilities.iter().any(|name| name == &spec.name) {
            return Err(HostError::invalid(format!(
                "capability spec `{}` is not in capabilities",
                spec.name
            )));
        }
        spec.validate()?;
        if !spec.tool_name.is_empty() && !tools.insert(spec.tool_name.as_str()) {
            return Err(HostError::invalid(format!(
                "tool name `{}` is declared twice",
                spec.tool_name
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Client {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) granted_capabilities: Vec<String>,
    pub(crate) capability_specs: Vec<CapabilitySpec>,
    pub(crate) revision: u64,
    pub(crate) last_seen: String,
}

impl Client {
    pub(crate) fn is_granted(&self, capability: &str) -> bool {
        self.granted_capabilities
            .iter()
            .any(|name| name == capability)
    }

    pub(crate) fn spec(&self, capability: &str) -> Option<&CapabilitySpec> {
        self.capability_specs
            .iter()
            .find(|spec| spec.name == capability)
    }

    /// The capabilities that give the parent a tool.
    pub(crate) fn tool_specs(&self) -> impl Iterator<Item = &CapabilitySpec> {
        self.capability_specs
            .iter()
            .filter(|spec| !spec.tool_name.is_empty() && self.is_granted(&spec.name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Activation {
    pub(crate) companion_id: String,
    pub(crate) client_id: Option<String>,
    /// The generation that binds audio, perception and device operations.
    pub(crate) revision: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeviceStatus {
    #[default]
    Pending,
    Dispatched,
    Completed,
    Failed,
    Uncertain,
}

impl DeviceStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatched => "dispatched",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pending" => Self::Pending,
            "dispatched" => Self::Dispatched,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "uncertain" => Self::Uncertain,
            _ => return None,
        })
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Uncertain)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceOperation {
    pub(crate) id: String,
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
    pub(crate) capability: String,
    pub(crate) input: serde_json::Value,
    pub(crate) work_id: Option<String>,
    pub(crate) status: DeviceStatus,
    pub(crate) output: Option<serde_json::Value>,
    pub(crate) failure: Option<Failure>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

/// Validates `input` against the declared schema of `spec`.
pub(crate) fn validate_input(spec: &CapabilitySpec, input: &serde_json::Value) -> Result<()> {
    let schema = json::parse_schema(&spec.input_schema)?;
    json::validate(&schema, input).map_err(HostError::invalid)
}

/// Validates a client's operation output against its declared output schema.
pub(crate) fn validate_output(spec: &CapabilitySpec, output: &serde_json::Value) -> Result<()> {
    if spec.output_schema.trim().is_empty() {
        return Ok(());
    }
    let schema = json::parse_schema(&spec.output_schema)?;
    json::validate(&schema, output).map_err(HostError::invalid)
}

/// Fills the approval-card target from the client's template and the verified
/// input. A template that references a field the input lacks leaves that part
/// empty; the host never trusts a client-supplied target.
pub(crate) fn approval_target(spec: &CapabilitySpec, input: &serde_json::Value) -> String {
    if spec.approval_target.trim().is_empty() {
        return format!("{} {}", spec.name, summarise(input));
    }
    let mut rendered = String::new();
    let mut rest = spec.approval_target.as_str();
    while let Some(start) = rest.find('{') {
        rendered.push_str(&rest[..start]);
        let inner = &rest[start + 1..];
        let Some(end) = inner.find('}') else {
            rendered.push_str(&rest[start..]);
            return rendered;
        };
        let key = &inner[..end];
        let value = input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        rendered.push_str(value);
        rest = &inner[end + 1..];
    }
    rendered.push_str(rest);
    rendered
}

fn summarise(input: &serde_json::Value) -> String {
    super::db::truncate(&input.to_string(), 200)
}
// Clients: capability declarations, activation generations and perception.
//
// The host stores capability names, JSON Schema, presets and approval
// declarations only. A declared capability with a `tool_name` becomes a tool
// for the parent while it is granted and the client is active; every
// operation is still gated by `permission::gate` and dispatched to the client
// as a `DeviceOperation`.
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
        self.all || self.allowed.iter().any(|allowed| allowed == capability)
    }

    /// The declared capabilities this policy grants, in declaration order.
    pub(crate) fn grants(&self, declared: &[String]) -> Vec<String> {
        declared
            .iter()
            .filter(|capability| self.allows(capability))
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
            "Run a capability the active client declared. The host binds the target client. \
             The validated result arrives in a later model call.",
            ToolOptions::default(),
            device::on_device_request,
        )
        .insert_resource(dynamic::CapabilityCalls::new(wake))
        .init_resource::<CapabilityPolicy>()
        .init_resource::<CapabilityTools>()
        // The agents spawn in `Startup` too; publish after them so a restored
        // active client's tools are on every parent from the first turn.
        .add_systems(Startup, dynamic::init.after(super::agent::reconcile))
        .add_systems(
            Update,
            (
                dynamic::drain.after(AgentLoop),
                dynamic::refresh.before(AgentLoop),
            ),
        );
    }
}

/// Every registered client, with its live connection state.
pub(crate) fn clients(store: &Store, hub: &DeviceHub) -> Result<Vec<w::Client>> {
    let activations = store.activations()?;
    Ok(store
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
    Ok(specs)
}

/// Publishes the client view, refreshes the capability tools and returns it.
fn publish(world: &mut World, client: &Client) -> Result<w::Client> {
    let connected = world.resource::<DeviceHub>().connected(&client.id);
    let view = wire::client(client, connected, Vec::new());
    notify_client(world, &view);
    dynamic::sync_tools(world)?;
    Ok(view)
}

/// Registers or refreshes a client declaration. A client never sends grants:
/// the host's [`CapabilityPolicy`] decides them, and the operation-level gate
/// still decides every call.
pub(crate) fn register(world: &mut World, request: w::RegisterClientRequest) -> Result<w::Client> {
    let store = world.resource::<Store>().clone();
    let specs = declarations(&request.capability_specs, &request.capabilities)?;
    let granted = world
        .resource::<CapabilityPolicy>()
        .grants(&request.capabilities);
    let client = store.register_client_with_grants(
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
    publish(world, &client)
}

pub(crate) fn set_capabilities(
    world: &mut World,
    request: w::SetCapabilitiesRequest,
) -> Result<w::Client> {
    let store = world.resource::<Store>().clone();
    let specs = declarations(&request.capability_specs, &request.capabilities)?;
    if !request.granted_capabilities.is_empty() {
        return Err(HostError::permission(
            "a client cannot grant capabilities to itself",
        ));
    }
    let granted = world
        .resource::<CapabilityPolicy>()
        .grants(&request.capabilities);
    let client = store.set_capabilities(
        &request.client_id,
        &request.capabilities,
        &granted,
        &specs,
        request.revision,
    )?;
    publish(world, &client)
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

/// Moves or releases the active client. A move invalidates the old generation:
/// its open operations fail as `client_moved`, its perception is dropped and
/// its approvals are expired.
pub(crate) fn activate(world: &mut World, request: w::ActivationRequest) -> Result<w::Activation> {
    let store = world.resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    if !request.release {
        super::db::validate_id(&request.client_id)?;
        store
            .client(&request.client_id)?
            .ok_or_else(|| HostError::not_found("client not found"))?;
    }
    let previous = store.activation(&request.companion_id)?;
    let client = (!request.release).then_some(request.client_id.as_str());
    let activation = store.activate(&request.companion_id, client, request.expected_revision)?;
    if let Some(previous) = previous
        && let Some(old_client) = previous.client_id
        && (client != Some(old_client.as_str()) || previous.revision != activation.revision)
    {
        store.fail_operations_of(&old_client, previous.revision)?;
        store.expire_approvals_for(&request.companion_id)?;
        // The ECS holds and waiting calls of the old generation must be
        // released too, or their turns would wait forever.
        super::permission::invalidate_generation(
            world,
            &request.companion_id,
            &old_client,
            previous.revision,
        );
        device::invalidate(world, &old_client, previous.revision);
        super::voice::on_client_moved(world, &request.companion_id, &old_client, previous.revision);
        world
            .resource_mut::<Perception>()
            .events
            .remove(&request.companion_id);
        // The old client keeps its stream but is no longer the active one.
        if let Some(old) = store.client(&old_client)? {
            let hub = world.resource::<DeviceHub>();
            notify_client(
                world,
                &wire::client(&old, hub.connected(&old.id), Vec::new()),
            );
        }
    }
    world.resource_mut::<Notifier>().push(
        Some(&request.companion_id),
        None,
        w::notification::Event::ActivationUpdated(Box::new(wire::activation(&activation))),
    );
    dynamic::sync_tools(world)?;
    Ok(wire::activation(&activation))
}

/// Projects a perception event into the companion's dynamic tail. A stale
/// generation is discarded.
pub(crate) fn report_perception(world: &mut World, event: w::PerceptionEvent) -> Result<()> {
    let store = world.resource::<Store>().clone();
    super::db::validate_id(&event.client_id)?;
    store
        .client(&event.client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    let Some((companion_id, revision)) = store.activation_for_client(&event.client_id)? else {
        // A connected client that is not active produces nothing the head may
        // act on. Not an error: the event is simply discarded.
        return Ok(());
    };
    if event.activation_revision != revision {
        return Ok(());
    }
    let Some(summary) = perception_summary(&event) else {
        return Ok(());
    };
    world
        .resource_mut::<Perception>()
        .events
        .insert(companion_id, PerceptionEvent { summary });
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
    };
    Some(super::db::truncate(&summary, 1_000))
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
    fn approval_targets_expand_verified_input_without_a_stray_brace() {
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
    fn only_the_host_policy_turns_declarations_into_grants() {
        let declared = vec!["body.motion".to_owned(), "device.files".to_owned()];
        assert!(CapabilityPolicy::none().grants(&declared).is_empty());
        assert!(CapabilityPolicy::parse("  , ").grants(&declared).is_empty());
        assert_eq!(
            CapabilityPolicy::parse("body.motion, body.motion").grants(&declared),
            vec!["body.motion".to_owned()]
        );
        assert_eq!(CapabilityPolicy::parse("*").grants(&declared), declared);
        assert!(
            CapabilityPolicy::parse("payment.transfer")
                .grants(&["body.motion".to_owned()])
                .is_empty()
        );
    }
}
