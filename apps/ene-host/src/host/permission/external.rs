//! Authority for an exact external tool registration, from Gate through dispatch.
use super::{
    gate::{GateCall, GateContext},
    scope_tag,
};
use crate::host::{
    error::{HostError, Result},
    providers::Running,
    resources::{ExternalToolOwner, HostExternalTool},
};
use bevy_ecs::prelude::Component;
use ene_policy::{
    CandidateAction, ExternalToolAction, ExternalToolSource, InvocationScope, canonical_json,
};
use rig_ecs::tools::CallHandler;
use std::sync::Arc;

#[derive(Component)]
pub(super) struct PreparedExternalTool {
    owner: ExternalToolOwner,
    pub(super) action: Arc<ExternalToolAction>,
}

impl PreparedExternalTool {
    pub(super) fn validate(&self) -> Result<()> {
        match &self.owner {
            ExternalToolOwner::Mcp(mcp) => mcp.validate(),
            ExternalToolOwner::Plugin(owner) if owner.revoked() => Err(HostError::Conflict(
                "the tool plugin was replaced or disabled".into(),
            )),
            ExternalToolOwner::Plugin(_) => Ok(()),
        }
    }

    pub(super) fn revoked(&self) -> bool {
        self.validate().is_err()
    }

    pub(super) fn candidate(&self) -> CandidateAction {
        CandidateAction::ExternalTool(Arc::clone(&self.action))
    }
}

pub(super) fn prepare_external(
    ctx: &GateContext,
    call: &GateCall,
    tool: &HostExternalTool,
    pinned: Option<&CallHandler>,
) -> Result<PreparedExternalTool> {
    if !pinned.is_some_and(|handler| handler.0.0.ptr_eq(&tool.handler)) {
        return Err(HostError::Conflict(
            "the external tool registration changed before approval".into(),
        ));
    }
    let key = ctx
        .credentials
        .as_ref()
        .and_then(|keys| keys.approval_key.as_ref());
    let (source, scope) = match &tool.owner {
        ExternalToolOwner::Mcp(registration) => (
            ExternalToolSource::Mcp {
                server_id: registration.server.clone(),
                receiver: registration.receiver().to_owned(),
            },
            key.and_then(|key| registration.scope(key, &call.tool_name, &call.arguments)),
        ),
        ExternalToolOwner::Plugin(owner) => (
            ExternalToolSource::Plugin(owner.plugin_id().to_owned()),
            key.and_then(|key| plugin_scope(key, owner, call)),
        ),
    };
    let prepared = PreparedExternalTool {
        owner: tool.owner.clone(),
        action: Arc::new(ExternalToolAction {
            source,
            tool_name: call.tool_name.clone(),
            input: call.arguments.clone(),
            scope,
        }),
    };
    prepared.validate()?;
    Ok(prepared)
}

fn plugin_scope(key: &[u8; 32], owner: &Running, call: &GateCall) -> Option<InvocationScope> {
    let revision = owner.package_revision();
    let plugin_id = owner.plugin_id();
    let tool_name = &call.tool_name;
    let tag = scope_tag(
        key,
        b"ene-host plugin tool v1\0",
        [
            plugin_id.as_bytes(),
            revision.as_bytes(),
            owner.installation_id().as_bytes(),
            tool_name.as_bytes(),
            canonical_json(&call.arguments).as_bytes(),
        ],
    )?;
    let label = format!(
        "{plugin_id} · {tool_name} · {}",
        revision.get(..12).unwrap_or(revision)
    );
    Some(InvocationScope::tool(tool_name.clone(), tag, label))
}
