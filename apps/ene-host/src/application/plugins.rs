use super::*;
impl HostApplication {
    pub async fn restore_plugins(&self) -> Result<(), Error> {
        if let Some(plugins) = &self.0.plugins {
            plugins.restore().await?;
        }
        Ok(())
    }
    fn plugin_port(&self) -> Result<&crate::infrastructure::plugins::Manager, Error> {
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
        let app = self.clone();
        self.owned(async move { app.plugin_port()?.reload().await })
            .await
    }
    pub async fn enable_plugin(
        &self,
        id: String,
        input: ene_rpc::EnablePluginRequest,
    ) -> Result<(), Error> {
        let app = self.clone();
        self.owned(async move { app.plugin_port()?.enable(id, input).await })
            .await
    }
    pub async fn plugin_bindings(&self) -> Result<Vec<ene_rpc::PluginBinding>, Error> {
        match &self.0.plugins {
            Some(plugins) => plugins.bindings().await,
            None => Ok(Vec::new()),
        }
    }
    pub async fn bind_plugin(&self, input: ene_rpc::PluginBinding) -> Result<(), Error> {
        let app = self.clone();
        self.owned(async move { app.bind_plugin_inner(input).await })
            .await
    }
    async fn bind_plugin_inner(&self, input: ene_rpc::PluginBinding) -> Result<(), Error> {
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
    pub async fn harness_state(
        &self,
        binding_id: String,
    ) -> Result<Option<crate::domain::harness::Session>, Error> {
        match &self.0.plugins {
            Some(plugins) => plugins.harness(binding_id).await,
            None => Ok(None),
        }
    }
    pub async fn invoke_plugin(
        &self,
        input: ene_rpc::PluginInvocation,
    ) -> Result<ene_rpc::PluginResult, Error> {
        let app = self.clone();
        self.owned(async move { app.invoke_plugin_inner(input).await })
            .await
    }
    async fn invoke_plugin_inner(
        &self,
        input: ene_rpc::PluginInvocation,
    ) -> Result<ene_rpc::PluginResult, Error> {
        let operation: ene_plugin::Operation =
            serde_json::from_str(&input.operation.json).map_err(|_| Error::invalid())?;
        self.invoke_operation(&input.binding_id, operation)
            .await?
            .into_result()
    }
    /// One operation of a bound plugin, in host types.
    async fn invoke_operation(
        &self,
        binding_id: &str,
        operation: ene_plugin::Operation,
    ) -> Result<crate::infrastructure::plugins::Completed, Error> {
        let bindings = self.plugin_port()?.bindings().await?;
        let binding = bindings
            .iter()
            .find(|b| b.id == binding_id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let completed = self
            .plugin_port()?
            .invoke(&binding.id, operation, Arc::new(self.clone()))
            .await?;
        if !self
            .plugin_port()?
            .bindings()
            .await?
            .iter()
            .any(|b| b == &binding)
        {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        let expires_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            + 5000;
        let mut boundary = self.0.boundary.lock().await;
        for event in &completed.events {
            if matches!(event, ene_plugin::Event::Character { .. }) {
                let document = serde_json::to_value(event)
                    .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
                generation::publish(
                    &mut boundary,
                    self,
                    Some(&binding.companion_id),
                    ene_rpc::notification::Event::PluginCharacterAction(Box::new(
                        ene_rpc::CharacterActionNotification {
                            binding_id: binding.id.clone(),
                            revision: binding.revision.clone(),
                            generation: completed.generation,
                            expires_at_ms: expires_at_ms as u64,
                            event: ene_rpc::JsonDocument::from(document).into(),
                            ..Default::default()
                        },
                    )),
                );
            }
        }
        // Harness turns publish a compact hint; the journal stays authoritative.
        if let crate::infrastructure::plugins::Answer::Harness(journal) = &completed.answer {
            generation::publish(
                &mut boundary,
                self,
                Some(&binding.companion_id),
                ene_rpc::notification::Event::HarnessTask(Box::new(
                    ene_rpc::HarnessTaskNotification {
                        binding_id: binding.id.clone(),
                        companion_id: binding.companion_id.clone(),
                        task_id: journal.session_id.clone(),
                        status: journal.status.name().into(),
                        stop_reason: journal
                            .stop_reason
                            .and_then(crate::infrastructure::plugins::stop_reason_name),
                        failure: None,
                        ..Default::default()
                    },
                )),
            );
        }
        Ok(completed)
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
                        let inner = app.clone();
                        let event = event.clone();
                        let _ = app
                            .owned(async move {
                                let operation = ene_plugin::Operation::Character {
                                    target: ene_plugin::CharacterTarget {
                                        device_id: binding.device_id.clone(),
                                        character_id: binding.companion_id.clone(),
                                    },
                                    event: event.clone(),
                                };
                                inner
                                    .invoke_operation(&binding.id, operation)
                                    .await
                                    .map(|_| ())
                            })
                            .await;
                    }
                }
            }
        });
    }
}

impl crate::ports::plugin::Mutations for HostApplication {
    fn submit(
        &self,
        operation: futures_util::future::BoxFuture<'static, Result<(), Error>>,
    ) -> futures_util::future::BoxFuture<'_, Result<(), Error>> {
        Box::pin(self.owned(operation))
    }
}
