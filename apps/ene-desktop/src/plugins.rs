//! Client-side Wasm plugin loading and capability execution (`F11`).
//!
//! Verified `Target::Client` packages under `<desktop_data_dir>/plugins/<id>/`
//! declare `Extension::Tool` capabilities exposed to the host as
//! `plugin.<plugin_id>.<tool_name>` with `CapabilityApproval::Confirm` so every
//! invocation still traverses the host's `CapabilityPolicy` and `Stage::Gate`.
use ene_plugin::{
    Extension, Operation, PluginToolDefinition, Target, ToolInvokeRequest, ToolInvokeResult,
};
use ene_plugin_runtime::{
    Package,
    instance::{DenyServices, Instance},
};
use ene_rpc as w;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const PLUGINS_DIR: &str = "plugins";
const PLUGINS_DIR_ENV: &str = "ENE_CLIENT_PLUGINS_DIR";

struct LoadedClientPlugin {
    entrypoint: String,
    package: Package,
    instance: Mutex<Option<Instance>>,
}

struct RegisteredClientTool {
    plugin: Arc<LoadedClientPlugin>,
    definition: PluginToolDefinition,
    capability: String,
}

#[derive(Default)]
pub struct ClientPlugins {
    tools: HashMap<String, RegisteredClientTool>,
    specs: Vec<w::ClientCapabilitySpec>,
}

impl ClientPlugins {
    /// Discovers verified `Target::Client` plugin packages under the desktop
    /// data directory (or `ENE_CLIENT_PLUGINS_DIR`). Runs synchronously at
    /// startup; plugins with invalid manifests or non-client targets are skipped.
    pub fn probe_default() -> Self {
        let Some(root) = plugins_dir() else {
            return Self::default();
        };
        if !root.is_dir() {
            return Self::default();
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(Self::load_from(&root))),
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(Self::load_from(&root)))
                .unwrap_or_default(),
        }
    }

    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::cognitive_complexity)]
    pub async fn load_from(directory: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Self::default();
        };
        let mut plugins = Self::default();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(package) = Package::load(&path) else {
                tracing::warn!(plugin = %path.display(), "skipping invalid client plugin package");
                continue;
            };
            let Some(ep) =
                package.manifest.entrypoints.iter().find(|ep| {
                    ep.target == Target::Client && ep.provides.contains(&Extension::Tool)
                })
            else {
                continue;
            };
            let plugin_id = package.manifest.id.clone();
            let entrypoint = ep.id.clone();
            let cancel = CancellationToken::new();
            let mut instance = match Instance::load_for_target(
                package.clone(),
                entrypoint.clone(),
                Target::Client,
                &cancel,
            )
            .await
            {
                Ok(instance) => instance,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "client plugin failed to load");
                    continue;
                }
            };
            let (tx, mut rx) = mpsc::channel(16);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let defs = match instance
                .invoke(Operation::ListTools, Arc::new(DenyServices), tx, &cancel)
                .await
                .and_then(|val| {
                    serde_json::from_value::<Vec<PluginToolDefinition>>(val)
                        .map_err(ene_plugin_runtime::Error::Json)
                }) {
                Ok(defs) => defs,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "client plugin ListTools failed");
                    continue;
                }
            };
            let loaded = Arc::new(LoadedClientPlugin {
                entrypoint,
                package,
                instance: Mutex::new(Some(instance)),
            });
            for def in defs {
                if def.name.trim().is_empty()
                    || w::BUILTIN_TOOLS.contains(&def.name.as_str())
                    || def
                        .name
                        .bytes()
                        .any(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
                {
                    continue;
                }
                let capability = format!("plugin.{plugin_id}.{}", def.name);
                if capability.chars().count() > 64 || plugins.tools.contains_key(&capability) {
                    continue;
                }
                let input_schema = serde_json::to_string(&def.parameters)
                    .unwrap_or_else(|_| r#"{"type":"object"}"#.into());
                plugins.specs.push(w::ClientCapabilitySpec {
                    name: capability.clone(),
                    tool_name: def.name.clone(),
                    description: def.description.clone(),
                    input_schema,
                    approval: w::CapabilityApproval::Confirm.into(),
                    ..Default::default()
                });
                plugins.tools.insert(
                    capability.clone(),
                    RegisteredClientTool {
                        plugin: Arc::clone(&loaded),
                        definition: def,
                        capability,
                    },
                );
            }
        }
        plugins
    }

    pub fn specs(&self) -> &[w::ClientCapabilitySpec] {
        &self.specs
    }

    pub async fn execute(&self, capability: &str, input: Value) -> Result<Value, String> {
        let Some(tool) = self.tools.get(capability) else {
            return Err(format!(
                "client plugin capability `{capability}` is not installed"
            ));
        };
        let mut slot = tool.plugin.instance.lock().await;
        let cancel = CancellationToken::new();
        if slot.as_ref().is_none_or(|i| !i.is_usable()) {
            let fresh = Instance::load_for_target(
                tool.plugin.package.clone(),
                tool.plugin.entrypoint.clone(),
                Target::Client,
                &cancel,
            )
            .await
            .map_err(|e| format!("client plugin reload failed: {e}"))?;
            *slot = Some(fresh);
        }
        let instance = slot
            .as_mut()
            .ok_or_else(|| "client plugin instance unavailable".to_owned())?;
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let raw = instance
            .invoke(
                Operation::InvokeTool {
                    request: ToolInvokeRequest {
                        name: tool.definition.name.clone(),
                        arguments: input,
                    },
                },
                Arc::new(DenyServices),
                tx,
                &cancel,
            )
            .await
            .map_err(|e| format!("client plugin `{}` failed: {e}", tool.capability))?;
        let parsed: ToolInvokeResult = serde_json::from_value(raw)
            .map_err(|e| format!("invalid client plugin tool result: {e}"))?;
        if parsed.is_error {
            Err(parsed.output.to_string())
        } else {
            Ok(parsed.output)
        }
    }
}

fn plugins_dir() -> Option<PathBuf> {
    if let Some(env_dir) = std::env::var_os(PLUGINS_DIR_ENV)
        && !env_dir.is_empty()
    {
        return Some(PathBuf::from(env_dir));
    }
    directories::ProjectDirs::from("", "", "ene")
        .map(|dirs| dirs.data_local_dir().join(PLUGINS_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn f11_loads_client_target_plugin_declares_spec_and_executes_tool() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let wasm = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join("release")
            .join("ene_reference_wasm.wasm");
        assert!(
            wasm.exists(),
            "missing {}; run `just build-test-plugins` first",
            wasm.display()
        );
        let temp = tempfile::tempdir().unwrap();
        let plugin_dir = temp.path().join("client-echo");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::copy(&wasm, plugin_dir.join("main.wasm")).unwrap();
        let manifest = serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "client-echo",
            "name": "Client Echo",
            "version": "1.0.0",
            "entrypoints": [{
                "id": "main",
                "target": "client",
                "file": "main.wasm",
                "provides": ["tool"],
                "uses": []
            }]
        });
        std::fs::write(plugin_dir.join("manifest.json"), manifest.to_string()).unwrap();

        let plugins = ClientPlugins::load_from(temp.path()).await;
        assert_eq!(plugins.specs().len(), 1);
        assert_eq!(plugins.specs()[0].name, "plugin.client-echo.reference_echo");
        assert_eq!(plugins.specs()[0].tool_name, "reference_echo");
        let output = plugins
            .execute(
                "plugin.client-echo.reference_echo",
                serde_json::json!({"text": "from desktop"}),
            )
            .await
            .unwrap();
        assert_eq!(output["echo"], "from desktop");
    }
}
