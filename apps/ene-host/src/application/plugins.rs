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
    pub fn plugins(&self) -> Vec<ene_rpc::Plugin> {
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
        input: ene_rpc::EnablePluginRequest,
    ) -> Result<(), Error> {
        self.plugin_port()?.enable(id, input).await
    }
    pub async fn plugin_bindings(&self) -> Result<Vec<ene_rpc::PluginBinding>, Error> {
        match &self.0.plugins {
            Some(plugins) => plugins.bindings().await,
            None => Ok(Vec::new()),
        }
    }
    pub async fn bind_plugin(&self, input: ene_rpc::PluginBinding) -> Result<(), Error> {
        let companion = input.companion_id.clone();
        let id = input.id.clone();
        self.plugin_port()?.bind(input).await?;
        self.character_event_for(
            Some(companion),
            Some(id),
            ene_plugin::CharacterEvent::Activated,
        );
        Ok(())
    }
    pub async fn invoke_plugin(
        &self,
        input: ene_rpc::PluginInvocation,
    ) -> Result<ene_rpc::PluginResult, Error> {
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
            .any(|b| b == binding)
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
            let value: serde_json::Value = serde_json::from_str(&event.json)
                .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("character") {
                generation::publish(
                    &mut boundary,
                    self,
                    Some(&binding.companion_id),
                    ene_rpc::notification::Event::PluginCharacterAction(Box::new(
                        ene_rpc::CharacterActionNotification {
                            binding_id: binding.id.clone(),
                            revision: binding.revision.clone(),
                            generation: result.generation,
                            expires_at_ms: expires_at_ms as u64,
                            event: event.clone().into(),
                            ..Default::default()
                        },
                    )),
                );
            }
        }
        Ok(result)
    }
    pub(crate) fn character_event(&self, companion: &str, event: ene_plugin::CharacterEvent) {
        self.character_event_for(Some(companion.into()), None, event);
    }
    fn character_event_for(
        &self,
        companion: Option<String>,
        binding_id: Option<String>,
        event: ene_plugin::CharacterEvent,
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
                                    && e.provides.iter().any(|extension| extension == "character")
                            })
                    }) {
                        let _ = app
                            .invoke_plugin(ene_rpc::PluginInvocation {
                                binding_id: binding.id,
                                operation: ene_rpc::JsonDocument {
                                    json: serde_json::to_string(
                                        &ene_plugin::Operation::Character {
                                            target: ene_plugin::CharacterTarget {
                                                device_id: binding.device_id.clone(),
                                                character_id: binding.companion_id.clone(),
                                            },
                                            event: event.clone(),
                                        },
                                    )
                                    .expect("serializable character operation"),
                                    ..Default::default()
                                }
                                .into(),
                                ..Default::default()
                            })
                            .await;
                    }
                }
            }
        });
    }
}
