//! The registration and receiver a tool call is authorized to use.
use super::{
    settings::Desired,
    supervisor::{Launch, Mcp},
};
use crate::host::{
    db::Store,
    error::{HostError, Result},
    model::Credentials,
    permission::scope_tag,
};
use ene_policy::{InvocationScope, canonical_json};
use rig_core::{
    completion::ToolDefinition,
    effect::{Answered, EffectId, EffectKind, Key, family},
    error::{ErrorKind, ErrorReport},
    serve::{AfterServe, BeforeServe, Intercept},
};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Receiver {
    pub(super) label: Arc<str>,
    tag: Option<[u8; 32]>,
}

impl Receiver {
    pub(super) fn new(
        id: &str,
        want: &Desired,
        secret: Option<&str>,
        key: Option<&[u8; 32]>,
    ) -> Self {
        let config = &want.config;
        let command = config.command.trim();
        let address = if command.is_empty() {
            url::Url::parse(config.endpoint.endpoint.trim()).map_or_else(
                |_| "HTTP MCP".into(),
                |url| url.origin().ascii_serialization(),
            )
        } else {
            command
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(command)
                .to_owned()
        };
        let tag = key.and_then(|key| {
            let config = serde_json::to_value(config).ok()?;
            scope_tag(
                key,
                b"ene-host MCP receiver v1\0",
                [
                    id.as_bytes(),
                    &want.stamp.created_revision.to_be_bytes(),
                    &want.stamp.revision.to_be_bytes(),
                    canonical_json(&config).as_bytes(),
                    secret.unwrap_or_default().as_bytes(),
                ],
            )
        });
        Self {
            label: format!("{} · {address}", want.name).into(),
            tag,
        }
    }
}

pub(crate) struct McpRegistration {
    pub(crate) server: String,
    pub(super) launch: Launch,
    pub(super) receiver: Receiver,
    pub(super) definition: Arc<ToolDefinition>,
    pub(super) key: Key<family::Tool>,
    pub(super) store: Store,
    pub(super) credentials: Option<Credentials>,
    pub(super) mcp: Mcp,
}

impl McpRegistration {
    pub(crate) fn receiver(&self) -> &str {
        &self.receiver.label
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self
            .launch
            .stamp
            .current(&self.store, self.credentials.as_ref(), &self.server)?
            && self.mcp.tool_is_current(self)
        {
            Ok(())
        } else {
            Err(HostError::Conflict(
                "the MCP connection or tool changed".into(),
            ))
        }
    }

    pub(crate) fn scope(
        &self,
        key: &[u8; 32],
        tool_name: &str,
        input: &Value,
    ) -> Option<InvocationScope> {
        let definition = serde_json::to_value(&*self.definition).ok()?;
        let tag = scope_tag(
            key,
            b"ene-host MCP tool v1\0",
            [
                self.receiver.tag.as_ref()?.as_slice(),
                canonical_json(&definition).as_bytes(),
                tool_name.as_bytes(),
                canonical_json(input).as_bytes(),
            ],
        )?;
        Some(InvocationScope::tool(
            tool_name.to_owned(),
            tag,
            format!("{} · {tool_name}", self.receiver()),
        ))
    }
}

/// Recheck even a handler that Rig pinned before the connection was replaced.
pub(super) struct ServeRegisteredTool(pub(super) Arc<McpRegistration>);

impl Intercept for ServeRegisteredTool {
    fn name(&self) -> String {
        "ene MCP registration".into()
    }

    async fn before(&self, _: EffectId, _: &EffectKind) -> BeforeServe {
        match self.0.validate() {
            Ok(()) => BeforeServe::Proceed,
            Err(error) => BeforeServe::deny(error.to_string()),
        }
    }

    async fn after(
        &self,
        _: EffectId,
        _: &EffectKind,
        _: &std::result::Result<Answered, ErrorReport>,
    ) -> AfterServe {
        match self.0.validate() {
            Ok(()) => AfterServe::Keep,
            Err(error) => {
                AfterServe::Replace(Err(
                    ErrorReport::new(ErrorKind::Denied, error.to_string()).refused()
                ))
            }
        }
    }
}
