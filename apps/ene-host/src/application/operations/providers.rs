use super::*;
impl HostApplication {
    pub fn providers(&self) -> Vec<crate::llm::Descriptor> {
        self.0.providers.descriptors()
    }
    pub(in crate::application) async fn reload_providers_inner(&self) -> Result<(), Error> {
        {
            let controls = self.0.controls.lock().await;
            for run in controls.running.values() {
                run.cancel.send_replace(true);
            }
        }
        self.0.providers.reload().await?;
        let mut controls = self.0.controls.lock().await;
        let mut c = config(self, &mut controls).await?;
        invalidate_changed_grants(self, &mut c)?;
        commit(self, &mut controls, c).await
    }
    pub(in crate::application) async fn enable_provider_inner(
        &self,
        id: String,
        enabled: bool,
    ) -> Result<(), Error> {
        // Cancel before changing availability; control paths never wait for inference.
        {
            let controls = self.0.controls.lock().await;
            if let Some(c) = &controls.config {
                for p in c.settings.connections.iter().filter(|p| p.adapter == id) {
                    cancel_connection(&controls, &p.id);
                }
            }
        }
        self.0.providers.enable(id, enabled).await?;
        let mut controls = self.0.controls.lock().await;
        let c = config(self, &mut controls).await?;
        commit(self, &mut controls, c).await
    }
    pub async fn models(
        &self,
        id: String,
        companion_id: Option<String>,
    ) -> Result<Vec<crate::llm::Model>, Error> {
        ConnectionId::parse(id.clone())?;
        let mut controls = self.0.controls.lock().await;
        controls.admit(self)?;
        let c = config(self, &mut controls).await?;
        let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let companion = companion_id
            .or_else(|| c.companions.keys().next().cloned())
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        if !c.companions.contains_key(&companion) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        let context = authorize(
            self,
            &c,
            &controls,
            &companion,
            &p,
            OperationKind::ModelList,
        )?;
        let job = Uuid::new_v4().to_string();
        let mut cancelled = register(
            &mut controls,
            job.clone(),
            companion,
            &p,
            &context,
            OperationKind::ModelList,
        );
        let input = invocation(
            self,
            p,
            context,
            job.clone(),
            InvocationInput::ModelList,
            cancelled.clone(),
        );
        let app = self.clone();
        let (mut reply, response) = oneshot::channel();
        // Registration and task ownership happen under the same controls lock as shutdown.
        self.0.generations.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = reply.closed() => Err(Error::new(ErrorKind::PermissionRequired)),
                result = execute(&app, OperationKind::ModelList, &mut cancelled, input.models()) => result,
            };
            let mut controls = app.0.controls.lock().await;
            let result = result.and_then(|models| { check_run(&app, &controls, &job)?; Ok(models) });
            // The final authority check, removal and reply are one serialized completion.
            if let Some(run) = controls.running.remove(&job) { run.cancel.send_replace(true); }
            let _ = reply.send(result);
        });
        drop(controls);
        response.await.unwrap_or_else(|_| Err(Error::unavailable()))
    }
}
