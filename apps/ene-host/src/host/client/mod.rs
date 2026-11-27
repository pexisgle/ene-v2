//! The domain of a declared client: capabilities, grants, the active client
//! and device operations.

use super::error::{HostError, Result};
use ene_domain::json;

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
pub(crate) mod device_dispatch;
pub(crate) mod device_result;
pub(crate) mod dynamic;
mod perception;

use super::{
    db::Store,
    notify::Notifier,
    resources::{DeviceHub, Perception},
    wire,
};
use bevy_ecs::prelude::*;
use dynamic::PublicationChange;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::Value;

pub(crate) use perception::ingest_perception;
#[cfg(test)]
pub(crate) use perception::report_perception;

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
            |called: On<ToolCalled<DeviceRequestArgs>>, commands: Commands| {
                device::on_device_request(called.event(), commands);
            },
        )
        .add_systems(
            AgentPass,
            (device::describe_device_request, dynamic::describe)
                .chain()
                .in_set(PrepareSystems::Edit),
        )
        .insert_resource(dynamic::CapabilityCalls::new(wake))
        .init_resource::<computer::ComputerUse>()
        .init_resource::<super::eyes::Eyes>()
        .add_systems(
            Update,
            (
                device::settle_disconnections.before(AgentLoop),
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
                .map(|activation| activation.companion_id.as_str().to_owned())
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
    for spec in &specs {
        for (side, text) in [
            ("input", &spec.input_schema),
            ("output", &spec.output_schema),
        ] {
            json::check_schema(&json::parse_schema(text)?).map_err(|_| {
                HostError::Invalid(format!(
                    "capability `{}` has an invalid {side} schema",
                    spec.name
                ))
            })?;
        }
    }
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

/// Two ways a client can update its declaration.
#[derive(Clone, Copy)]
pub(crate) enum Declaration<'a> {
    Register(&'a w::RegisterClientRequest),
    Replace(&'a w::SetCapabilitiesRequest),
}

pub(crate) fn declare(world: &mut World, declaration: Declaration<'_>) -> Result<w::Client> {
    let (id, capabilities, wire_specs) = match declaration {
        Declaration::Register(request) => (
            &request.id,
            &request.capabilities,
            &request.capability_specs,
        ),
        Declaration::Replace(request) => (
            &request.client_id,
            &request.capabilities,
            &request.capability_specs,
        ),
    };
    let specs = declarations(wire_specs, capabilities)?;
    if let Declaration::Replace(request) = declaration
        && !request.granted_capabilities.is_empty()
    {
        return Err(HostError::Permission(
            "a client cannot grant capabilities to itself".into(),
        ));
    }
    let store = world.resource::<Store>().clone();
    let repo = store.client_repo();
    let previous = repo.client(id)?;
    let client = match declaration {
        Declaration::Register(request) => {
            repo.register_client(&super::db::clients::ClientDeclaration {
                id,
                name: &request.name,
                platform: &request.platform,
                capabilities,
                specs: &specs,
                expected_revision: request.expected_revision,
            })?
        }
        Declaration::Replace(request) => {
            repo.set_capabilities(id, capabilities, &specs, request.revision)?
        }
    };
    let result = (|| -> Result<w::Client> {
        let active = repo.active_companions_for_client(id)?;
        let view = wire::client(&client, world.resource::<DeviceHub>().connected(id), active);
        dynamic::sync_tools(world, PublicationChange::Client(id))?;
        if !client.is_granted(w::COMPUTER_USE) {
            computer::invalidate_client(world, id, None);
        }
        notify_client(world.resource::<Notifier>(), &view);
        Ok(view)
    })();
    if result.is_err() {
        if let Some(previous) = previous {
            ene_domain::warn_if_err(
                repo.restore_client(&previous),
                "client declaration rollback",
            );
        } else if let Declaration::Register(_) = declaration {
            ene_domain::warn_if_err(repo.delete_client(id), "rejected client cleanup");
        }
        ene_domain::warn_if_err(
            dynamic::sync_tools(world, PublicationChange::Refresh),
            "tool resync",
        );
    }
    result
}

pub(super) fn notify_client(notifier: &Notifier, client: &w::Client) {
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
pub(crate) fn activate(world: &mut World, request: &w::ActivationRequest) -> Result<w::Activation> {
    let store = world.resource::<Store>().clone();
    store
        .companion_repo()
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    if !request.release {
        ene_domain::validate_id(&request.client_id)?;
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
    if let Err(error) =
        dynamic::sync_tools(world, PublicationChange::Companion(&request.companion_id))
    {
        if let Some(previous) = previous.as_ref() {
            ene_domain::warn_if_err(
                store.client_repo().restore_activation(previous),
                "activation rollback",
            );
        } else {
            ene_domain::warn_if_err(
                store.client_repo().delete_activation(&request.companion_id),
                "rejected activation cleanup",
            );
        }
        ene_domain::warn_if_err(
            dynamic::sync_tools(world, PublicationChange::Refresh),
            "tool resync",
        );
        return Err(error);
    }
    if let Some(previous) = previous
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
            let view = {
                let hub = world.resource::<DeviceHub>();
                let active = store.client_repo().active_companions_for_client(&old.id)?;
                wire::client(&old, hub.connected(&old.id), active)
            };
            notify_client(world.resource::<Notifier>(), &view);
        }
    }
    if !request.release {
        super::voice::on_client_activated(
            world.get_resource::<super::voice::Voice>(),
            &request.companion_id,
            &request.client_id,
        );
    }
    world.resource::<Notifier>().push(
        Some(&request.companion_id),
        None,
        w::notification::Event::ActivationUpdated(Box::new((&activation).into())),
    );
    Ok((&activation).into())
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
    fn a_declaration_rejects_unusable_input_and_output_schemas() {
        let invalid = r#"{"type":"not-a-valid-json-schema-type"}"#.to_owned();
        let declared = ["body.motion".to_owned()];
        let spec = w::ClientCapabilitySpec {
            name: declared[0].clone(),
            tool_name: "body_motion".into(),
            ..Default::default()
        };
        let input = w::ClientCapabilitySpec {
            input_schema: invalid.clone(),
            ..spec.clone()
        };
        let output = w::ClientCapabilitySpec {
            output_schema: invalid,
            ..spec.clone()
        };
        assert!(
            declarations(&[input], &declared)
                .unwrap_err()
                .to_string()
                .contains("invalid input schema")
        );
        assert!(
            declarations(&[output], &declared)
                .unwrap_err()
                .to_string()
                .contains("invalid output schema")
        );
        assert!(declarations(&[spec], &declared).is_ok());
    }
}
