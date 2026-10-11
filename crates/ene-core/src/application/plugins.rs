use super::*;
impl HostApplication {
    pub async fn restore_plugins(&self) {
        if let Some(plugins) = &self.0.plugins {
            plugins.restore().await;
        }
    }
    fn plugin_port(&self) -> Result<&dyn crate::ports::plugin::Plugins, Error> {
        self.0
            .plugins
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))
    }
    pub fn plugins(&self) -> Vec<ene_api_types::plugin::PluginInfo> {
        self.0
            .plugins
            .as_ref()
            .map(|p| p.list())
            .unwrap_or_default()
    }
    pub async fn reload_plugins(&self) -> Result<(), Error> {
        self.plugin_port()?.reload().await
    }
    pub async fn enable_plugin(
        &self,
        id: String,
        input: ene_api_types::plugin::PluginEnabled,
    ) -> Result<(), Error> {
        self.plugin_port()?.enable(id, input).await
    }
    pub async fn plugin_bindings(
        &self,
    ) -> Result<Vec<ene_api_types::plugin::PluginBinding>, Error> {
        match &self.0.plugins {
            Some(plugins) => plugins.bindings().await,
            None => Ok(Vec::new()),
        }
    }
    pub async fn bind_plugin(
        &self,
        input: ene_api_types::plugin::PluginBinding,
    ) -> Result<(), Error> {
        let companion = input.companion_id.clone();
        let id = input.id.clone();
        self.plugin_port()?.bind(input).await?;
        self.character_event_for(
            Some(companion),
            Some(id),
            serde_json::json!({"type":"activated"}),
        );
        Ok(())
    }
    pub async fn invoke_plugin(
        &self,
        input: ene_api_types::plugin::PluginInvocation,
    ) -> Result<ene_api_types::plugin::PluginResult, Error> {
        let bindings = self.plugin_port()?.bindings().await?;
        let binding = bindings
            .iter()
            .find(|b| b.id == input.binding_id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let result = self.plugin_port()?.invoke(input).await?;
        if !self
            .plugin_port()?
            .bindings()
            .await?
            .iter()
            .any(|b| serde_json::to_value(b).ok() == serde_json::to_value(binding).ok())
        {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        let expires_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            + 5000;
        let mut boundary = self.0.boundary.lock().await;
        for event in &result.events {
            if event.get("type").and_then(serde_json::Value::as_str) == Some("character") {
                generation::publish(
                    &mut boundary,
                    self,
                    "plugin.character_action",
                    Some(&binding.companion_id),
                    serde_json::json!({"binding_id":binding.id,"revision":binding.revision,"generation":result.generation,"expires_at_ms":expires_at_ms,"event":event}),
                );
            }
        }
        Ok(result)
    }
    pub(crate) fn character_event(&self, companion: &str, event: serde_json::Value) {
        self.character_event_for(Some(companion.into()), None, event);
    }
    fn character_event_for(
        &self,
        companion: Option<String>,
        binding_id: Option<String>,
        event: serde_json::Value,
    ) {
        let Ok(permit) = self.0.plugin_events.clone().try_acquire_owned() else {
            return;
        };
        if self.0.plugins.is_none() || self.0.plugin_tasks.is_closed() {
            return;
        }
        let app = self.clone();
        self.0.plugin_tasks.spawn(async move {
            let _permit = permit;
            if let Ok(bindings) = app.plugin_bindings().await {
                let plugins = app.plugins();
                for binding in bindings {
                    if companion
                        .as_ref()
                        .is_some_and(|id| id != &binding.companion_id)
                    {
                        continue;
                    }
                    if binding_id.as_ref().is_some_and(|id| &binding.id != id) {
                        continue;
                    }
                    if plugins.iter().any(|p| {
                        p.id == binding.plugin_id
                            && p.state == "ready"
                            && p.entrypoints.iter().any(|e| {
                                e.id == binding.entrypoint
                                    && e.provides
                                        .contains(&ene_api_types::plugin::Extension::Character)
                            })
                    }) {
                        let _ = app
                            .invoke_plugin(ene_api_types::plugin::PluginInvocation {
                                binding_id: binding.id,
                                operation: serde_json::json!({"type":"character","event":event}),
                            })
                            .await;
                    }
                }
            }
        });
    }
}
