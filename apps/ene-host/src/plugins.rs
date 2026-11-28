use crate::{actor::Actor, configuration::save, error::invalid, plugin_runtime::Package, store};
use anyhow::Result;
use ene_rpc as w;

pub(crate) trait PluginCommands {
    async fn put_plugin(&mut self, request: w::PluginChange) -> Result<w::Plugin>;
}

impl PluginCommands for Actor {
    async fn put_plugin(&mut self, request: w::PluginChange) -> Result<w::Plugin> {
        let mutation = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_plugin", &request))?;
        let mut plugin = request
            .plugin
            .into_option()
            .ok_or_else(|| invalid("plugin required"))?;
        let path = std::path::PathBuf::from(&plugin.path);
        let package = tokio::task::spawn_blocking(move || Package::load(&path)).await??;
        if !plugin.id.is_empty() && plugin.id != package.manifest.id {
            return Err(invalid("plugin package ID differs from its registration"));
        }
        plugin.id = package.manifest.id.clone();
        plugin.label = package.manifest.name.clone();
        plugin.package_digest = package.revision.clone();
        plugin.revision = mutation.revision + 1;
        plugin.destinations = package
            .manifest
            .entrypoints
            .iter()
            .flat_map(|entry| entry.provider.iter())
            .flat_map(|provider| provider.destinations.iter().cloned())
            .collect();
        plugin.destinations.sort();
        plugin.destinations.dedup();
        plugin.capabilities = package
            .manifest
            .entrypoints
            .iter()
            .flat_map(|entry| entry.provides.iter())
            .map(|extension| format!("{extension:?}"))
            .collect();
        plugin.capabilities.sort();
        plugin.capabilities.dedup();
        let id = plugin.id.clone();
        save(&self.db, "plugin", mutation, &id, &hash, plugin).await
    }
}

pub(crate) async fn hooks(
    db: &sqlx::SqlitePool,
    event: ene_plugin::HookEvent,
    cancel: &tokio_util::sync::CancellationToken,
) -> Vec<ene_plugin::HookDecision> {
    let work = async {
        let mut read = db.acquire().await?;
        let plugins: Vec<w::Plugin> = store::list(&mut read, "plugin").await?;
        drop(read);
        let mut replies = Vec::new();
        for plugin in plugins
            .into_iter()
            .filter(|plugin| {
                plugin.enabled
                    && plugin
                        .capabilities
                        .iter()
                        .any(|capability| capability == "Hook")
            })
            .take(8)
        {
            let path = std::path::PathBuf::from(&plugin.path);
            let Ok(Ok(package)) = tokio::task::spawn_blocking(move || Package::load(&path)).await
            else {
                continue;
            };
            if package.revision != plugin.package_digest {
                continue;
            }
            for entry in &package.manifest.entrypoints {
                if entry.target != ene_plugin::Target::Host
                    || !entry.provides.contains(&ene_plugin::Extension::Hook)
                {
                    continue;
                }
                let reply = crate::plugin_tools::invoke(
                    &package,
                    &entry.id,
                    ene_plugin::Operation::Hook {
                        event: event.clone(),
                    },
                    cancel,
                )
                .await;
                if let Ok((value, _)) = reply
                    && let Ok(decision) = serde_json::from_value(value)
                {
                    replies.push(decision);
                }
            }
        }
        Ok::<_, anyhow::Error>(replies)
    };
    match tokio::time::timeout(std::time::Duration::from_millis(500), work).await {
        Ok(Ok(decisions)) => decisions,
        _ => Vec::new(),
    }
}

pub(crate) trait ToolObservation {
    async fn observe_tool(
        &self,
        operation: &crate::approval::Operation,
        tool: &str,
        result: Option<bool>,
    );
}

impl ToolObservation for crate::actor::Host {
    async fn observe_tool(
        &self,
        operation: &crate::approval::Operation,
        tool: &str,
        result: Option<bool>,
    ) {
        let db = self
            .call(|actor| Box::pin(async { Ok(actor.db.clone()) }))
            .await;
        let Ok(db) = db else {
            return;
        };
        let companion_id = operation.companion.clone();
        let tool_name = tool.to_owned();
        let event = match result {
            Some(is_error) => ene_plugin::HookEvent::PostTool {
                companion_id,
                tool_name,
                is_error,
            },
            None => ene_plugin::HookEvent::PreTool {
                companion_id,
                tool_name,
            },
        };
        let _observed = hooks(&db, event, &operation.cancel).await;
    }
}
