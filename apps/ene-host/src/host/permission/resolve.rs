//! Resolving one tool call before its verdict: the saved rules it is judged
//! against, the pre-tool hooks it must pass, and the `CandidateAction` it names.
use super::approval::refuse;
use super::gate::{GateCall, GateContext};
use crate::host::{
    client::Client,
    db::Store,
    error::{HostError, Result},
};
use bevy_ecs::prelude::*;
use ene_policy::{CandidateAction, CapabilitySpec, PolicyRule};
use ene_rpc as w;
use serde_json::Value;

pub(super) fn load_gate_rules(
    ctx: &mut GateContext,
) -> std::result::Result<Vec<PolicyRule>, ene_store::StoreError> {
    if let Some(rules) = &ctx.cached_rules {
        return Ok(rules.clone());
    }
    let rules = ctx.store.policy_repo().policy_rules()?;
    ctx.cached_rules = Some(rules.clone());
    Ok(rules)
}

pub(super) fn run_pre_tool_hooks(
    ctx: &GateContext,
    companion_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> Option<String> {
    let providers = ctx.providers.as_ref()?;
    if providers
        .with_extension(ene_plugin::Extension::Hook)
        .is_empty()
    {
        return None;
    }
    let runtime = ctx.runtime.as_ref()?;
    let hook_store = ctx.store.clone();
    let hook_event = ene_plugin::HookEvent::PreTool {
        companion_id: companion_id.to_owned(),
        tool_name: tool_name.to_owned(),
        arguments: arguments.clone(),
    };
    let outcome = tokio::task::block_in_place(|| {
        runtime.block_on(crate::host::providers::extensions::run_hooks(
            providers,
            &hook_store,
            hook_event,
        ))
    });
    outcome.blocked
}

type CandidateResolution = (CandidateAction, Option<String>, Option<String>);

pub(super) enum CandidateOutcome {
    Resolved(Box<CandidateResolution>),
    NeedsDeviceClient,
    Refuse(String),
}

pub(super) fn build_candidate_action(ctx: &GateContext, item: &GateCall) -> CandidateOutcome {
    if let Some(server_id) = ctx.mcp_tools.get(&item.tool_name) {
        return CandidateOutcome::Resolved(Box::new((
            CandidateAction::McpTool {
                server_id: server_id.clone(),
                tool_name: item.tool_name.clone(),
                input: item.arguments.clone(),
            },
            None,
            None,
        )));
    }
    if let Some(plugin_id) = ctx.plugin_tools.get(&item.tool_name) {
        return CandidateOutcome::Resolved(Box::new((
            CandidateAction::PluginTool {
                plugin_id: plugin_id.clone(),
                tool_name: item.tool_name.clone(),
                input: item.arguments.clone(),
            },
            None,
            None,
        )));
    }
    if item.tool_name == crate::host::web_search::NAME {
        let providers = ctx.providers.clone().unwrap_or_default();
        let resolved = match crate::host::web_search::resolve(
            &ctx.store,
            ctx.credentials.as_ref(),
            &providers,
        ) {
            Ok(resolved) => resolved,
            Err(reason) => return CandidateOutcome::Refuse(reason.to_string()),
        };
        let query = item
            .arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return CandidateOutcome::Resolved(Box::new((
            CandidateAction::WebSearch {
                endpoint: resolved.endpoint,
                query,
                input: item.arguments.clone(),
            },
            None,
            None,
        )));
    }
    CandidateOutcome::NeedsDeviceClient
}

pub(super) fn resolve_device_candidate(
    world: &mut World,
    store: &Store,
    companion_id: &str,
    work_id: Option<&str>,
    agent_ctx: &AgentCapabilityContext,
    item: &GateCall,
) -> Option<CandidateResolution> {
    let client_id = match store.client_repo().target(companion_id, work_id) {
        Ok(Some(client_id)) => client_id,
        Ok(None) => {
            refuse(
                world,
                item.entity,
                "no_active_client: no client is bound to this agent",
            );
            return None;
        }
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            } else {
                refuse(
                    world,
                    item.entity,
                    "client binding could not be read; failing closed",
                );
            }
            return None;
        }
    };
    let client = match store.client_repo().client(&client_id) {
        Ok(Some(client)) => client,
        Ok(None) => {
            refuse(world, item.entity, "the bound client is not registered");
            return None;
        }
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            } else {
                refuse(
                    world,
                    item.entity,
                    "client record could not be read; failing closed",
                );
            }
            return None;
        }
    };
    let dynamic_spec = if item.tool_name != "device_request" && !agent_ctx.is_work_agent {
        match crate::host::client::dynamic::tool_spec_for(world, &client, &item.tool_name) {
            Ok(spec) => spec,
            Err(error) => {
                refuse(world, item.entity, error.message());
                return None;
            }
        }
    } else {
        None
    };
    let resolved = match resolve_capability(
        agent_ctx,
        dynamic_spec,
        &client,
        &item.tool_name,
        &item.arguments,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            refuse(world, item.entity, error.message());
            return None;
        }
    };
    let Some((capability, spec)) = resolved else {
        refuse(
            world,
            item.entity,
            &format!("unknown device tool `{}`", item.tool_name),
        );
        return None;
    };
    if !client.is_granted(&capability) {
        refuse(
            world,
            item.entity,
            &format!("capability `{capability}` is not granted"),
        );
        return None;
    }
    let empty = serde_json::json!({});
    let input = if item.tool_name == "device_request" {
        item.arguments.get("input").unwrap_or(&empty).clone()
    } else {
        item.arguments.clone()
    };
    if (capability.starts_with(crate::host::client::WORKSPACE_CAPABILITY_PREFIX)
        || capability == w::COMPUTER_USE)
        && let Some(spec) = &spec
        && let Err(error) = crate::host::client::validate_input(spec, &input)
    {
        refuse(world, item.entity, error.message());
        return None;
    }
    Some((
        CandidateAction::DeviceCapability {
            capability: capability.clone(),
            spec: spec.map(Box::new),
            tool_name: item.tool_name.clone(),
            input,
            raw_arguments: item.arguments.clone(),
        },
        Some(client_id),
        Some(capability),
    ))
}

pub(super) struct AgentCapabilityContext {
    pub(super) is_work_agent: bool,
    pub(super) computer_model_supported: bool,
}

/// The capability a device tool call names, with its declaration.
fn resolve_capability(
    agent_ctx: &AgentCapabilityContext,
    dynamic_spec: Option<CapabilitySpec>,
    client: &Client,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<(String, Option<CapabilitySpec>)>> {
    if tool_name == "device_request" {
        let Some(capability) = arguments.get("capability").and_then(Value::as_str) else {
            return Err(HostError::Invalid(
                ("device_request requires a capability name as a string").into(),
            ));
        };
        if !client.capabilities.iter().any(|name| name == capability) {
            return Err(HostError::Invalid(format!(
                "capability `{capability}` is not declared by the bound client"
            )));
        }
        if capability.starts_with(crate::host::client::WORKSPACE_CAPABILITY_PREFIX)
            && !agent_ctx.is_work_agent
        {
            return Err(HostError::Permission(
                ("workspace capabilities require a work session").into(),
            ));
        }
        if capability == w::COMPUTER_USE
            && (!agent_ctx.is_work_agent || !agent_ctx.computer_model_supported)
        {
            return Err(HostError::Permission(
                ("Computer Use requires an internal work agent with an image and tool capable task model").into(),
            ));
        }
        return Ok(Some((
            capability.to_owned(),
            client.spec(capability).cloned(),
        )));
    }
    if agent_ctx.is_work_agent {
        return Err(HostError::Permission(
            ("capability tools are offered to the parent only").into(),
        ));
    }
    let Some(spec) = dynamic_spec else {
        return Ok(None);
    };
    Ok(Some((spec.name.clone(), Some(spec))))
}
