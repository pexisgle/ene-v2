use crate::plugins::ToolObservation as _;

use crate::{
    actor::Host,
    approval::Operation,
    error::{conflict, denied},
    model::truncate,
    plugin_runtime::{
        Package,
        instance::{DenyServices, Instance},
    },
    store,
};
use anyhow::{Result, ensure};
use ene_core::policy::{Action, Effect, Target};
use ene_plugin::{Event, Extension, PluginToolDefinition, ToolInvokeRequest, ToolInvokeResult};
use ene_rpc::Plugin;
use rig_core::{completion::ToolDefinition, message::ToolName};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct PluginTool {
    plugin: Plugin,
    package: Package,
    entrypoint: String,
    definition: PluginToolDefinition,
}

pub(crate) async fn list_tools(host: &Host, cancel: &CancellationToken) -> Result<Vec<PluginTool>> {
    let plugins: Vec<Plugin> = host
        .call(|actor| {
            Box::pin(async {
                let mut db = actor.db.acquire().await?;
                store::list(&mut db, "plugin").await
            })
        })
        .await?;
    let mut tools = Vec::new();
    for plugin in plugins.into_iter().filter(|plugin| plugin.enabled).take(12) {
        let path = std::path::PathBuf::from(&plugin.path);
        let package = tokio::task::spawn_blocking(move || Package::load(&path)).await?;
        let Ok(package) = package else {
            continue;
        };
        if package.revision != plugin.package_digest {
            continue;
        }
        for entry in &package.manifest.entrypoints {
            if entry.target != ene_plugin::Target::Host
                || !entry.provides.contains(&Extension::Tool)
            {
                continue;
            }
            let result = invoke(
                &package,
                &entry.id,
                ene_plugin::Operation::ListTools,
                cancel,
            )
            .await;
            let Ok((reply, _)) = result else {
                continue;
            };
            let Ok(definitions) = serde_json::from_value::<Vec<PluginToolDefinition>>(reply) else {
                continue;
            };
            for definition in definitions.into_iter().take(24) {
                if !definition.parameters.is_object()
                    || serde_json::to_vec(&definition.parameters)?.len() > 12_000
                {
                    continue;
                }
                tools.push(PluginTool {
                    package: package.clone(),
                    entrypoint: entry.id.clone(),
                    plugin: plugin.clone(),
                    definition,
                });
            }
        }
    }
    Ok(tools)
}

impl PluginTool {
    pub(crate) fn definition(&self, alias: &str) -> Result<ToolDefinition> {
        Ok(ToolDefinition::new(
            ToolName::new(alias)?,
            format!(
                "{}: {}",
                truncate(&self.definition.name, 100),
                truncate(&self.definition.description, 600)
            ),
            self.definition.parameters.clone(),
        ))
    }

    pub(crate) async fn call(
        &self,
        host: &Host,
        operation: &Operation,
        arguments: Map<String, Value>,
    ) -> Result<String> {
        ensure!(
            jsonschema::is_valid(
                &self.definition.parameters,
                &Value::Object(arguments.clone())
            ),
            "plugin arguments do not match schema"
        );
        let action = Action::new(
            Target {
                receiver: format!("plugin/{}/{}", self.plugin.id, self.entrypoint),
                revision: store::fingerprint(&(
                    &self.plugin.package_digest,
                    self.plugin.revision,
                    &self.definition,
                ))?,
                operation: self.definition.name.clone(),
                effect: if self.definition.read_only {
                    Effect::Read
                } else {
                    Effect::Execute
                },
                device: None,
            },
            arguments,
        )?;
        host.observe_tool(operation, &self.definition.name, None)
            .await;
        let digest = host.authorize(operation, action.clone()).await?;
        action.verify(&digest)?;
        let plugin = self.plugin.clone();
        host.call(move |actor| {
            Box::pin(async move {
                let mut db = actor.db.acquire().await?;
                let current: Option<Plugin> = store::get(&mut db, "plugin", &plugin.id).await?;
                if !current.is_some_and(|current| {
                    current.enabled
                        && current.revision == plugin.revision
                        && current.package_digest == plugin.package_digest
                }) {
                    return Err(conflict("plugin changed before invocation"));
                }
                Ok(())
            })
        })
        .await?;
        if operation.cancel.is_cancelled() {
            return Err(denied("operation cancelled"));
        }
        let (value, _) = invoke(
            &self.package,
            &self.entrypoint,
            ene_plugin::Operation::InvokeTool {
                request: ToolInvokeRequest {
                    name: self.definition.name.clone(),
                    arguments: action.arguments().clone().into(),
                },
            },
            &operation.cancel,
        )
        .await?;
        let result: ToolInvokeResult = serde_json::from_value(value)?;
        host.observe_tool(operation, &self.definition.name, Some(result.is_error))
            .await;
        ensure!(
            !result.is_error,
            "plugin reported an unsuccessful operation"
        );
        Ok(serde_json::to_string(&result.output)?)
    }
}

pub(crate) async fn invoke(
    package: &Package,
    entrypoint: &str,
    operation: ene_plugin::Operation,
    cancel: &CancellationToken,
) -> Result<(Value, Vec<Event>)> {
    let mut instance = Instance::load(package.clone(), entrypoint.to_owned(), cancel).await?;
    let (send, mut events) = mpsc::channel(16);
    let call = instance.invoke(operation, Arc::new(DenyServices), send, cancel);
    let receive = async {
        let mut values = Vec::new();
        while let Some(event) = events.recv().await {
            values.push(event);
        }
        values
    };
    let (result, events) = tokio::join!(call, receive);
    Ok((result?, events))
}
