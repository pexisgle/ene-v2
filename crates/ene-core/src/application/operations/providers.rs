use super::*;
impl HostApplication {
    pub fn providers(&self) -> Vec<crate::ports::llm::Descriptor> {
        self.0.providers.descriptors()
    }
    pub async fn reload_providers(&self) -> Result<(), Error> {
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
    pub async fn enable_provider(&self, id: String, enabled: bool) -> Result<(), Error> {
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
        let secret = permitted_data(
            self,
            &c,
            &controls,
            &companion_id,
            &p,
            "connection_check",
            &["metadata"],
        )?;
        if !allowed(
            self,
            &c,
            &companion_id,
            &p,
            "connection_check",
            &["metadata"],
        ) {
            return Err(Error::new(ErrorKind::PermissionRequired));
        }
        if controls.running.len() >= 8 {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        let job = Uuid::new_v4().to_string();
        let (cancel, mut cancelled) = watch::channel(false);
        controls.running.insert(
            job.clone(),
            Running {
                companion: companion_id,
                connection: id,
                data: vec!["metadata"],
                policy: self.0.providers.policy(&p)?,
                terminal_error: None,
                purpose: "connection_check",
                cancel,
            },
        );
        drop(controls);
        let request = crate::ports::llm::Generation {
            endpoint: p.endpoint.clone(),
            options: p.options.clone(),
            model: String::new(),
            preamble: String::new(),
            messages: vec![],
            max_tokens: 0,
        };
        let input = invocation(
            self,
            p,
            secret,
            job.clone(),
            request,
            Operation::ModelList,
            cancelled.clone(),
        );
        let app = self.clone();
        let (mut reply, response) = oneshot::channel();
        // Own cleanup outside the caller: dropping an HTTP request cancels the work,
        // but cannot skip releasing its execution slot.
        tokio::spawn(async move {
            let mut shutdown = app.shutdown();
            let result = if *cancelled.borrow() || *shutdown.borrow() {
                Err(Error::unavailable())
            } else {
                tokio::select! {
                    biased;
                    _ = reply.closed() => Err(Error::unavailable()),
                    _ = shutdown.changed() => Err(Error::unavailable()),
                    _ = cancelled.changed() => Err(Error::unavailable()),
                    r = tokio::time::timeout(std::time::Duration::from_secs(30), app.0.providers.models(input)) => r.unwrap_or_else(|_| Err(Error::unavailable())),
                }
            };
            let mut controls = app.0.controls.lock().await;
            if let Some(run) = controls.running.remove(&job) {
                run.cancel.send_replace(true);
            }
            let _ = reply.send(result);
        });
        response.await.unwrap_or_else(|_| Err(Error::unavailable()))
    }
}
