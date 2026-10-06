use super::*;

impl HostApplication {
    pub(in crate::application) async fn initialize_inner(&self) -> Result<(), Error> {
        let initial = {
            let mut controls = self.0.controls.lock().await;
            config(self, &mut controls).await?
        };
        let mut loaded = BTreeMap::new();
        // Bounded concurrency and one startup deadline, outside every control lock.
        let loader = self.clone();
        let mut loading = futures_util::stream::iter(initial.credential_refs.clone())
            .map(move |(id, reference)| {
                let loader = loader.clone();
                async move { (id, loader.0.credentials.load(reference).await) }
            })
            .buffer_unordered(8);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some((id, value))) = tokio::time::timeout_at(deadline, loading.next()).await {
            if let Ok(value) = value
                && !value.is_empty()
                && value.len() <= 8192
                && !value.chars().any(char::is_control)
            {
                loaded.insert(
                    id,
                    Secret {
                        value,
                        nonce: Uuid::new_v4().to_string(),
                    },
                );
            }
        }
        drop(loading);
        let mut controls = self.0.controls.lock().await;
        let mut c = config(self, &mut controls).await?;
        for mut connection in c.settings.connections.clone() {
            if !controls.secrets.contains_key(&connection.id)
                && c.credential_refs.get(&connection.id)
                    == initial.credential_refs.get(&connection.id)
                && rules::connection(&initial, &connection.id).as_ref() == Some(&connection)
                && let Some(secret) = loaded.remove(&connection.id)
            {
                controls.secrets.insert(connection.id.clone(), secret);
            }
            connection.credential_state = if controls.secrets.contains_key(&connection.id) {
                if c.credential_refs.contains_key(&connection.id) {
                    "stored"
                } else {
                    "session_only"
                }
            } else {
                "missing"
            }
            .into();
            update_connection(&mut c, connection);
        }
        for grant in &mut c.grants {
            if grant.status == GrantStatus::Active
                && c.settings
                    .connections
                    .iter()
                    .find(|connection| connection.id == grant.connection_id)
                    .is_none_or(|connection| {
                        !self
                            .0
                            .providers
                            .policy(connection)
                            .is_ok_and(|policy| policy.fingerprint == grant.connection_fingerprint)
                    })
            {
                grant.status = GrantStatus::Invalidated;
                grant.invalidation_reason = Some("provider_changed".into());
                grant.revision = next_revision(grant.revision)?;
            }
        }
        commit(self, &mut controls, c).await
    }
}
