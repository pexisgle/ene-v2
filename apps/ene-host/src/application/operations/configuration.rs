use super::*;
pub async fn patch_settings(
    app: HostApplication,
    request: SettingsPatch,
) -> Result<Settings, Error> {
    // Old clients used tag 2 for a repeated list. Do not silently accept a
    // discarded legacy replacement as a successful no-op.
    if request
        .__buffa_unknown_fields
        .iter()
        .any(|field| field.number == 2)
    {
        return Err(Error::invalid());
    }
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let inputs = request.connections.into_option();
    if request.revision != c.settings.revision {
        // A replay of an already-applied settings write must keep its stable IDs.
        if request.revision < c.settings.revision
            && inputs.as_ref().is_some_and(|list| {
                list.items.len() == c.settings.connections.len()
                    && list
                        .items
                        .iter()
                        .zip(&c.settings.connections)
                        .all(|(input, saved)| {
                            input.id.as_deref() == Some(saved.id.as_str())
                                && input.kind == saved.kind
                                && input.adapter == saved.adapter
                                && input.endpoint == saved.endpoint
                                && input.options == saved.options
                        })
            })
        {
            return Ok(c.settings);
        }
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    let Some(inputs) = inputs else {
        return Ok(c.settings);
    };
    let mut connections = vec![];
    let mut ids = std::collections::HashSet::new();
    let mut credential_resets = std::collections::HashSet::new();
    for input in inputs.items {
        if input.kind != "tts"
            && input.id.as_ref().is_some_and(|id| {
                c.companions
                    .values()
                    .any(|companion| voice_connection(companion).as_ref() == Some(id))
            })
        {
            return Err(Error::new(ErrorKind::StateConflict));
        }
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
                capabilities: Default::default(),
                last_check: Default::default(),
                ..Default::default()
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
            capabilities: previous
                .as_ref()
                .map(|p| p.capabilities.clone())
                .unwrap_or_default(),
            last_check: previous
                .as_ref()
                .map(|p| p.last_check.clone())
                .unwrap_or_default(),
            ..Default::default()
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
                g.revision = next_revision(g.revision)?;
            }
            p.capabilities = Default::default();
            p.last_check = Default::default();
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
                    .any(|a| a.as_option().is_some_and(|a| a.connection_id == id))
                })
                || c.companions
                    .values()
                    .any(|companion| voice_connection(companion).as_deref() == Some(id))
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
    c.settings.revision = next_revision(c.settings.revision)?;
    commit(&app, &mut controls, c.clone()).await?;
    controls
        .secrets
        .retain(|id, _| ids.contains(id) && !credential_resets.contains(id));
    for reference in removed_refs {
        app.schedule_forget(reference);
    }
    Ok(c.settings)
}

pub async fn patch_companion(
    app: HostApplication,
    id: String,
    request: CompanionPatch,
) -> Result<Companion, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let mut companion = c
        .companions
        .get(&id)
        .cloned()
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    if companion.revision != request.revision {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if let Some(name) = request.name {
        if name.trim().is_empty() || name.len() > 200 {
            return Err(Error::invalid());
        }
        companion.name = name;
    }
    if let Some(mind) = request.mind {
        if mind.len() > 16_000 {
            return Err(Error::invalid());
        }
        companion.mind = mind;
    }
    if let Some(body) = document_change(request.body.into_option())? {
        if let Some(document) = &body {
            let environment = app.0.repository.snapshot().await?.environment_id;
            validate_body(document, &environment)?;
        }
        companion.body = body.into();
    }
    if let Some(voice) = document_change(request.voice.into_option())? {
        if let Some(document) = &voice {
            validate_voice(document, &c)?;
        }
        companion.voice = voice.into();
    }
    for (task, patch) in [
        (false, request.companion_assignment),
        (true, request.task_assignment),
    ] {
        let Some(patch) = patch.into_option() else {
            continue;
        };
        let assignment = match patch.change {
            Some(ene_rpc::assignment_patch::Change::Set(assignment)) => {
                let p = rules::connection(&c, &assignment.connection_id)
                    .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
                app.0.providers.policy(&p)?;
                validate_model_id(&assignment.model_id)?;
                Some(*assignment)
            }
            Some(ene_rpc::assignment_patch::Change::Clear(_)) => None,
            None => return Err(Error::invalid()),
        };
        let assignments = companion.model_assignments.get_or_insert_default();
        if task {
            assignments.task = assignment.into();
        } else {
            assignments.companion = assignment.into();
        }
    }
    companion.revision = next_revision(companion.revision)?;
    c.companions.insert(id.clone(), companion);
    refresh(&app, &controls, &mut c);
    commit(&app, &mut controls, c.clone()).await?;
    Ok(c.companions.remove(&id).expect("validated companion"))
}

fn document_change(
    patch: Option<ene_rpc::JsonDocumentPatch>,
) -> Result<Option<Option<ene_rpc::JsonDocument>>, Error> {
    use ene_rpc::json_document_patch::Change;
    match patch.map(|patch| patch.change) {
        None => Ok(None),
        Some(Some(Change::Set(document))) => Ok(Some(Some(*document))),
        Some(Some(Change::Clear(_))) => Ok(Some(None)),
        Some(None) => Err(Error::invalid()),
    }
}

#[derive(serde::Deserialize)]
struct BodyDocument {
    format: String,
    environment_id: String,
    model: String,
}

#[derive(serde::Deserialize)]
struct VoiceDocument {
    connection_id: String,
    voice_id: String,
}

#[derive(serde::Deserialize)]
struct VoiceConnection {
    connection_id: String,
}

fn parse_document<T: serde::de::DeserializeOwned>(
    document: &ene_rpc::JsonDocument,
) -> Result<T, Error> {
    serde_json::from_str(&document.json).map_err(|_| Error::invalid())
}

fn non_blank(value: &str) -> Result<&str, Error> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(Error::invalid());
    }
    Ok(value)
}

fn validate_body(document: &ene_rpc::JsonDocument, environment: &str) -> Result<(), Error> {
    let body: BodyDocument = parse_document(document)?;
    if body.format != "vrm1" {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    if body.environment_id != environment {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    non_blank(&body.model)?;
    Ok(())
}

fn validate_voice(
    document: &ene_rpc::JsonDocument,
    configuration: &Configuration,
) -> Result<(), Error> {
    let voice: VoiceDocument = parse_document(document)?;
    let id = non_blank(&voice.connection_id)?;
    non_blank(&voice.voice_id)?;
    let connection =
        rules::connection(configuration, id).ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
    if connection.kind != "tts" {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    Ok(())
}

fn voice_connection(companion: &Companion) -> Option<String> {
    let voice: VoiceConnection = parse_document(companion.voice.as_option()?).ok()?;
    non_blank(&voice.connection_id).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document(json: Value) -> ene_rpc::JsonDocument {
        ene_rpc::JsonDocument {
            json: json.to_string(),
            ..Default::default()
        }
    }
    #[test]
    fn body_rejects_wrong_environment_format_and_non_object_documents() {
        assert!(
            validate_body(
                &document(json!({"format":"vrm1","environment_id":"e","model":"avatar.vrm"})),
                "e"
            )
            .is_ok()
        );
        for invalid in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"format":"vrm1","environment_id":"other","model":"avatar.vrm"}),
            json!({"format":"unsupported","environment_id":"e","model":"avatar.vrm"}),
            json!({"format":"vrm1","environment_id":"e","model":" "}),
        ] {
            assert!(validate_body(&document(invalid), "e").is_err());
        }
        assert!(
            validate_body(
                &ene_rpc::JsonDocument {
                    json: "{broken".into(),
                    ..Default::default()
                },
                "e"
            )
            .is_err()
        );
    }
    #[test]
    fn voice_requires_an_existing_tts_connection_and_a_voice_reference() {
        let mut c = Configuration {
            companions: Default::default(),
            settings: Settings {
                connections: vec![
                    Connection {
                        id: "tts".into(),
                        kind: "tts".into(),
                        ..Default::default()
                    },
                    Connection {
                        id: "llm".into(),
                        kind: "llm".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            grants: vec![],
            credential_refs: Default::default(),
        };
        let voice = document(json!({"connection_id":"tts","voice_id":"voice1"}));
        assert!(validate_voice(&voice, &c).is_ok());
        for value in [
            json!({"connection_id":"missing","voice_id":"voice1"}),
            json!({"connection_id":"llm","voice_id":"voice1"}),
            json!({"connection_id":"tts","voice_id":""}),
            Value::Null,
        ] {
            assert!(validate_voice(&document(value), &c).is_err());
        }
        c.settings.connections.clear();
        assert!(validate_voice(&voice, &c).is_err());
    }
}
