use super::*;
impl HostApplication {
    pub async fn reload_providers(&self) -> Result<(), Error> {
        let app = self.clone();
        self.execute(async move { app.apply_reload_providers().await })
            .await
    }
    pub async fn enable_provider(&self, id: String, enabled: bool) -> Result<(), Error> {
        let app = self.clone();
        self.execute(async move { app.apply_enable_provider(id, enabled).await })
            .await
    }

    pub fn providers(&self) -> Vec<crate::ports::llm::Descriptor> {
        self.0.providers.descriptors()
    }
    async fn apply_reload_providers(&self) -> Result<(), Error> {
        {
            let controls = self.0.controls.lock().await;
            for run in controls.running.values() {
                run.cancel.send_replace(true);
            }
        }
        self.0.providers.reload().await?;
        let mut controls = self.0.controls.lock().await;
        let mut c = config(self, &mut controls).await?;
        for g in &mut c.grants {
            if g.status == GrantStatus::Active
                && c.settings
                    .connections
                    .iter()
                    .find(|p| p.id == g.connection_id)
                    .is_none_or(|p| {
                        fingerprint(p, self.0.providers.as_ref()) != g.connection_fingerprint
                    })
            {
                g.status = GrantStatus::Invalidated;
                g.invalidation_reason = Some("provider_changed".into());
                g.revision += 1;
            }
        }
        commit(self, &mut controls, c).await
    }
    async fn apply_enable_provider(&self, id: String, enabled: bool) -> Result<(), Error> {
        // Cancel before changing availability; control paths never wait for inference.
        {
            let controls = self.0.controls.lock().await;
            if let Some(c) = &controls.config {
                for p in c.settings.connections.iter().filter(|p| p.adapter == id) {
                    cancel_connection(&controls, &p.id);
                }
            }
        }
        let mut shutdown = self.shutdown();
        if *shutdown.borrow() {
            return Err(Error::unavailable());
        }
        tokio::select! {biased;_=shutdown.changed()=>return Err(Error::unavailable()),r=self.0.providers.enable(id,enabled)=>r?};
        let mut controls = self.0.controls.lock().await;
        let c = config(self, &mut controls).await?;
        commit(self, &mut controls, c).await
    }
    pub async fn models(
        &self,
        id: String,
        companion_id: Option<String>,
    ) -> Result<Vec<crate::ports::llm::Model>, Error> {
        let mut controls = self.0.controls.lock().await;
        let c = config(self, &mut controls).await?;
        let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let companion_id = companion_id
            .or_else(|| c.companions.keys().next().cloned())
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        if !c.companions.contains_key(&companion_id) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        let prepared = prepare(self, &c, &controls, &companion_id, &p, Operation::ModelList)?;
        let job = Uuid::new_v4().to_string();
        let mut cancelled = prepared.cancelled;
        let secret = prepared.secret;
        controls.running.insert(job.clone(), prepared.running);
        let request = crate::ports::llm::Generation {
            endpoint: p.endpoint.clone(),
            options: p.options.clone(),
            model: String::new(),
            preamble: String::new(),
            messages: vec![],
            max_tokens: 0,
        };
        let input = invocation(self, p, secret, job.clone(), request, cancelled.clone());
        let app = self.clone();
        let (mut reply, response) = oneshot::channel();
        // Own cleanup outside the caller: dropping an HTTP request cancels the work,
        // but cannot skip releasing its execution slot.
        self.0.generations.spawn(async move {
            let mut shutdown = app.shutdown();
            let work = async {
                tokio::select! {
                    biased;
                    _ = reply.closed() => Err(Error::unavailable()),
                    result = app.0.providers.models(input) => result,
                }
            };
            let mut result = Operation::ModelList
                .wait(&mut cancelled, &mut shutdown, work)
                .await
                .map_err(|failure| match failure {
                    RunFailure::Failed(error) => error,
                    _ => Error::unavailable(),
                });
            let mut controls = app.0.controls.lock().await;
            if is_cancelled(&app, &controls, &job) {
                result = Err(Error::unavailable());
            }
            if let Some(run) = controls.running.remove(&job) {
                run.cancel.send_replace(true);
            }
            let _ = reply.send(result);
        });
        drop(controls);
        response.await.unwrap_or_else(|_| Err(Error::unavailable()))
    }
}
