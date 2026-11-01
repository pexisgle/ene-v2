//! Binding reads and writes. A binding is the grant a host invocation runs under.
use super::{Manager, failure, name, storage_failure, verify};
use crate::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use ene_plugin::{Extension, Target};
use ene_rpc::PluginBinding;
use serde_json::Value;
use std::path::Path;

impl Manager {
    pub(super) async fn list_bindings(&self) -> Result<Vec<PluginBinding>> {
        self.store
            .plugin_list("bindings".into())
            .await
            .map_err(storage_failure)?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(failure))
            .collect()
    }

    pub(super) async fn bind_plugin(&self, mut input: PluginBinding) -> Result<()> {
        match input.workspace_root.take() {
            Some(root) if !root.is_empty() => {
                input.workspace_root = Some(workspace_root(&root)?);
            }
            _ => {
                input.workspace_root = None;
            }
        }
        let entry = self.ready(&input.plugin_id, Some(&input.revision))?;
        verify(entry.package.clone()).await?;
        let endpoint = entry.package.entry(&input.entrypoint).map_err(failure)?;
        if !ene_plugin_runtime::package::identifier(&input.id)
            || !matches!(endpoint.target, Target::Host | Target::Acp)
            || endpoint.provides.contains(&Extension::Character)
                && !ene_plugin_runtime::package::identifier(&input.device_id)
            || input
                .services
                .iter()
                .any(|s| !endpoint.uses.iter().any(|service| name(*service) == *s))
            || serde_json::to_vec(&input.configuration)
                .map_err(failure)?
                .len()
                > 65536
            || input.services.iter().any(|s| {
                !matches!(
                    s.as_str(),
                    "log" | "configuration" | "state" | "models" | "tools"
                )
            })
        {
            return Err(Error::invalid());
        }
        if endpoint.target == Target::Acp
            && (input.workspace_root.is_none() || !input.services.is_empty())
        {
            return Err(Error::invalid());
        }
        let terminal_enabled = input
            .terminal
            .as_option()
            .is_some_and(|policy| policy.enabled);
        if terminal_enabled {
            let policy = input.terminal.as_option().ok_or_else(Error::invalid)?;
            if endpoint.target != Target::Acp
                || input.workspace_root.is_none()
                || policy.allowed_commands.is_empty()
                || policy.allowed_commands.len() > 64
                || policy.allowed_commands.iter().any(|command| {
                    command.is_empty()
                        || command.len() > 4096
                        || command.chars().any(char::is_control)
                })
                || policy.max_seconds == 0
                || policy.max_seconds > 300
                || policy.max_output_bytes == 0
                || policy.max_output_bytes > 1024 * 1024
            {
                return Err(Error::invalid());
            }
        } else {
            input.terminal = Default::default();
        }
        serde_json::from_str::<Value>(&input.configuration.json).map_err(|_| Error::invalid())?;
        let snapshot = self.store.snapshot().await.map_err(storage_failure)?;
        if !snapshot
            .companions
            .iter()
            .any(|c| c.id == input.companion_id)
        {
            return Err(Error::new(ErrorKind::NotFound));
        }
        // Stop the old session and store the new binding as one step for invocations.
        let _binding = self.binding.lock().await;
        let session = self.sessions.lock().unwrap().get(&input.id).cloned();
        if let Some(session) = session {
            self.stop_session(&input.id, &session).await?;
        } else {
            self.harness_stop(&input.id, "plugin_stopped").await?;
        }
        self.stop_acp(&input.id, "plugin_stopped").await?;
        self.store
            .plugin_set(
                "bindings".into(),
                input.id.clone(),
                serde_json::to_value(input).map_err(failure)?,
            )
            .await
            .map_err(storage_failure)?;
        Ok(())
    }
}

/// Canonical absolute directory; the filesystem root is not a workspace.
fn workspace_root(root: &str) -> Result<String> {
    let path = Path::new(root);
    if !path.is_absolute() || root.len() > 4096 || root.chars().any(char::is_control) {
        return Err(Error::invalid());
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| Error::invalid())?;
    if canonical.parent().is_none() || !canonical.is_dir() {
        return Err(Error::invalid());
    }
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(Error::invalid)
}
