use super::*;
pub async fn patch_settings(
    app: HostApplication,
    request: SettingsPatch,
) -> Result<Settings, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if request.revision != c.settings.revision {
        // A replay of an already-applied settings write must keep its stable IDs.
        if request.revision < c.settings.revision
            && request.connections.len() == c.settings.connections.len()
            && request
                .connections
                .iter()
                .zip(&c.settings.connections)
                .all(|(input, saved)| {
                    input.id.as_deref() == Some(saved.id.as_str())
                        && input.kind == saved.kind
                        && input.adapter == saved.adapter
                        && input.endpoint == saved.endpoint
                        && input.options == saved.options
                })
        {
            return Ok(c.settings);
        }
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    let mut connections = vec![];
    let mut ids = std::collections::HashSet::new();
    let mut credential_resets = std::collections::HashSet::new();
    for input in request.connections {
        if input.create
            && input
                .id
                .as_ref()
                .is_none_or(|id| Uuid::parse_str(id).is_err())
        {
            return Err(Error::invalid());
        }
        let previous = input.id.as_ref().and_then(|id| rules::connection(&c, id));
        if input.id.is_some() && previous.is_none() && !input.create {
            return Err(Error::new(ErrorKind::NotFound));
        }
        if previous.as_ref().is_none_or(|old| {
            old.kind != input.kind
                || old.adapter != input.adapter
                || old.endpoint != input.endpoint
                || old.options != input.options
        }) {
            app.0.providers.policy(&ProviderConnection {
                id: String::new(),
                kind: input.kind.clone(),
                adapter: input.adapter.clone(),
                endpoint: input.endpoint.clone(),
                options: input.options.clone(),
                credential_state: "missing".into(),
                capabilities: None,
                last_check: None,
            })?;
        }
        let id = previous
            .as_ref()
            .map(|p| p.id.clone())
            .or(input.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if !ids.insert(id.clone()) {
            return Err(Error::invalid());
        }
        let mut p = ProviderConnection {
            id,
            kind: input.kind,
            adapter: input.adapter,
            endpoint: input.endpoint,
            options: input.options,
            credential_state: previous
                .as_ref()
                .map(|p| p.credential_state.clone())
                .unwrap_or("missing".into()),
            capabilities: previous.as_ref().and_then(|p| p.capabilities.clone()),
            last_check: previous.as_ref().and_then(|p| p.last_check.clone()),
        };
        if let Some(old) = &previous
            && (old.adapter != p.adapter || old.endpoint != p.endpoint)
        {
            credential_resets.insert(p.id.clone());
            p.credential_state = "missing".into();
        }
        if let Some(old) = previous
            && fingerprint(&old, app.0.providers.as_ref())
                != fingerprint(&p, app.0.providers.as_ref())
        {
            if active(&controls, &old.id) {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            for g in c
                .grants
                .iter_mut()
                .filter(|g| g.connection_id == old.id && g.status == GrantStatus::Active)
            {
                g.status = GrantStatus::Invalidated;
                g.invalidation_reason = Some("connection_changed".into());
                g.revision += 1;
            }
            p.capabilities = None;
            p.last_check = None;
        }
        connections.push(p);
    }
    for old in &c.settings.connections {
        let id = old.id.as_str();
        if !ids.contains(id)
            && (active(&controls, id)
                || c.companions.values().any(|companion| {
                    [
                        &companion.model_assignments.companion,
                        &companion.model_assignments.task,
                    ]
                    .iter()
                    .any(|a| a.as_ref().is_some_and(|a| a.connection_id == id))
                })
                || c.grants
                    .iter()
                    .any(|g| g.connection_id == id && g.status == GrantStatus::Active))
        {
            return Err(Error::new(ErrorKind::StateConflict));
        }
    }
    let removed_refs = c
        .credential_refs
        .iter()
        .filter(|(id, _)| !ids.contains(*id) || credential_resets.contains(*id))
        .map(|(_, reference)| reference.clone())
        .collect::<Vec<_>>();
    c.credential_refs
        .retain(|id, _| ids.contains(id) && !credential_resets.contains(id));
    c.settings.connections = connections;
    c.settings.revision += 1;
    commit(&app, &mut controls, c.clone()).await?;
    controls
        .secrets
        .retain(|id, _| ids.contains(id) && !credential_resets.contains(id));
    for reference in removed_refs {
        app.0.credentials.forget(reference);
    }
    Ok(c.settings)
}

pub(super) struct PatchCompanion {
    pub id: String,
    pub request: CompanionPatch,
}
impl crate::application::ConfigurationCommand for PatchCompanion {
    type Output = Companion;
    fn target(&self) -> crate::application::ConfigurationTarget {
        crate::application::ConfigurationTarget::Companion {
            id: self.id.clone(),
            revision: self.request.revision,
        }
    }
    fn apply(
        &self,
        c: &mut Configuration,
        providers: &dyn crate::ports::llm::ProviderRegistry,
    ) -> Result<(), Error> {
        let id = &self.id;
        let request = &self.request;
        let mut companion = c
            .companions
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        if let Some(name) = &request.name {
            if name.trim().is_empty() || name.len() > 200 {
                return Err(Error::invalid());
            }
            companion.name = name.clone();
        }
        if let Some(mind) = &request.mind {
            if mind.len() > 16_000 {
                return Err(Error::invalid());
            }
            companion.mind = mind.clone();
        }
        if let Some(assignments) = &request.model_assignments {
            for (key, assignment) in assignments {
                if !["companion", "task"].contains(&key.as_str()) {
                    return Err(Error::invalid());
                }
                if let Some(a) = assignment {
                    let p = rules::connection(c, &a.connection_id)
                        .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
                    providers.policy(&p)?;
                    if a.model_id.trim().is_empty() || a.model_id.len() > 512 {
                        return Err(Error::invalid());
                    }
                }
                if key == "companion" {
                    companion.model_assignments.companion = assignment.clone();
                } else {
                    companion.model_assignments.task = assignment.clone();
                }
            }
        }
        c.companions.insert(id.clone(), companion);
        Ok(())
    }
    fn output(&self, c: &Configuration) -> Result<Companion, Error> {
        c.companions
            .get(&self.id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }
}
