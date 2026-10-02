use super::HostApplication;
use super::generation::*;
use crate::{
    domain::{rules, *},
    ports::{Outcome, Receipt},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionInput {
    pub id: Option<String>,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub revision: u64,
    pub connections: Vec<ConnectionInput>,
}
pub async fn patch_settings(app: HostApplication, request: SettingsPatch) -> Result<Value, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if request.revision != c.settings.revision {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    let mut connections = vec![];
    let mut ids = std::collections::HashSet::new();
    for input in request.connections {
        let previous = input
            .id
            .as_ref()
            .map(|id| rules::connection(&c, id).ok_or_else(|| Error::new(ErrorKind::NotFound)))
            .transpose()?;
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
                || [
                    &c.companion.model_assignments.companion,
                    &c.companion.model_assignments.task,
                ]
                .iter()
                .any(|a| a.as_ref().is_some_and(|a| a.connection_id == id))
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
        .filter(|(id, _)| !ids.contains(*id))
        .map(|(_, reference)| reference.clone())
        .collect::<Vec<_>>();
    c.credential_refs.retain(|id, _| ids.contains(id));
    c.settings.connections = connections;
    c.settings.revision += 1;
    commit(&app, &mut controls, c.clone()).await?;
    controls.secrets.retain(|id, _| ids.contains(id));
    for reference in removed_refs {
        app.0.credentials.forget(reference);
    }
    Ok(json!(c.settings))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionPatch {
    pub revision: u64,
    pub name: Option<String>,
    pub mind: Option<String>,
    pub model_assignments: Option<std::collections::BTreeMap<String, Option<ModelAssignment>>>,
}
pub async fn patch_companion(
    app: HostApplication,
    id: String,
    request: CompanionPatch,
) -> Result<Value, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if c.companion.id != id {
        return Err(Error::new(ErrorKind::NotFound));
    }
    if c.companion.revision != request.revision {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if let Some(name) = request.name {
        if name.trim().is_empty() || name.len() > 200 {
            return Err(Error::invalid());
        }
        c.companion.name = name;
    }
    if let Some(mind) = request.mind {
        if mind.len() > 16_000 {
            return Err(Error::invalid());
        }
        c.companion.mind = mind;
    }
    if let Some(assignments) = request.model_assignments {
        for (key, assignment) in assignments {
            if !["companion", "task"].contains(&key.as_str()) {
                return Err(Error::invalid());
            }
            if let Some(a) = &assignment {
                let p = rules::connection(&c, &a.connection_id)
                    .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
                app.0.providers.policy(&p)?;
                if a.model_id.trim().is_empty() || a.model_id.len() > 512 {
                    return Err(Error::invalid());
                }
            }
            if key == "companion" {
                c.companion.model_assignments.companion = assignment;
            } else {
                c.companion.model_assignments.task = assignment;
            }
        }
    }
    c.companion.revision += 1;
    refresh(&app, &mut c);
    commit(&app, &mut controls, c.clone()).await?;
    Ok(json!(c.companion))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantInput {
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
}
pub async fn post_grant(
    app: HostApplication,
    key: String,
    request: GrantInput,
) -> Result<(Outcome, Value), Error> {
    let body = json!({"companion_id":request.companion_id,"kind":request.kind,"connection_id":request.connection_id,"destinations":request.destinations,"data_kinds":request.data_kinds,"purpose":request.purpose});
    let key_lookup = key.clone();
    let lookup = body.clone();
    let mut controls = app.0.controls.lock().await;
    if let Some(receipt) = app
        .0
        .repository
        .receipt(key_lookup, "/v1/grants".into(), lookup)
        .await?
    {
        return Ok(receipt);
    }
    let mut c = config(&app, &mut controls).await?;
    if request.companion_id != c.companion.id {
        return Err(Error::new(ErrorKind::NotFound));
    }
    let connection = rules::connection(&c, &request.connection_id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let policy = app.0.providers.policy(&connection)?;
    if request.kind != "data_transfer"
        || !["conversation", "connection_check"].contains(&request.purpose.as_str())
        || request
            .destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        || request
            .data_kinds
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        || request.destinations.is_empty()
        || request.data_kinds.is_empty()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let grant = Grant {
        id: Uuid::new_v4().to_string(),
        companion_id: request.companion_id,
        kind: request.kind,
        connection_id: request.connection_id,
        destinations: request.destinations,
        data_kinds: request.data_kinds,
        purpose: GrantPurpose::parse(&request.purpose)?,
        status: GrantStatus::Active,
        revision: 1,
        invalidation_reason: None,
        connection_fingerprint: fingerprint(&connection, app.0.providers.as_ref()),
    };
    c.grants.push(grant.clone());
    refresh(&app, &mut c);
    let response = json!({"grant":grant,"questions":[]});
    let saved = c.clone();
    let saved_response = response.clone();
    let mut boundary = app.0.boundary.lock().await;
    app.0
        .repository
        .save_configuration(
            saved,
            Some(Receipt {
                key,
                target: "/v1/grants".into(),
                request: body,
                outcome: Outcome::Created,
                response: saved_response,
            }),
        )
        .await?;
    controls.config = Some(c.clone());
    publish(&mut boundary, &app, "grants.updated", None, json!(c.grants));
    publish(
        &mut boundary,
        &app,
        "companion.updated",
        Some(&c.companion.id),
        json!(c.companion),
    );
    Ok((Outcome::Created, response))
}
pub async fn delete_grant(app: HostApplication, id: String) -> Result<(), Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if let Some(g) = c.grants.iter_mut().find(|g| g.id == id)
        && g.status == GrantStatus::Active
    {
        cancel_purpose(&controls, &g.connection_id, g.purpose.as_str());
        g.status = GrantStatus::Invalidated;
        g.revision += 1;
        g.invalidation_reason = Some("revoked".into());
        commit(&app, &mut controls, c).await?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPatch {
    pub revision: u64,
    pub destinations: Option<Vec<String>>,
    pub data_kinds: Option<Vec<String>>,
}
pub async fn patch_grant(
    app: HostApplication,
    id: String,
    request: GrantPatch,
) -> Result<Value, Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let connection_id = c
        .grants
        .iter()
        .find(|g| g.id == id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?
        .connection_id
        .clone();
    let p = rules::connection(&c, &connection_id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let policy = app.0.providers.policy(&p)?;
    let g = c
        .grants
        .iter_mut()
        .find(|g| g.id == id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    if g.revision != request.revision {
        return Err(Error::new(ErrorKind::RevisionConflict));
    }
    if g.status != GrantStatus::Active {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    if let Some(destinations) = request.destinations {
        if destinations
            .iter()
            .any(|d| !policy.destinations.contains(d))
        {
            return Err(Error::invalid());
        }
        g.destinations = destinations;
    }
    if let Some(data) = request.data_kinds {
        if data
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        {
            return Err(Error::invalid());
        }
        g.data_kinds = data;
    }
    cancel_purpose(&controls, &g.connection_id, g.purpose.as_str());
    g.revision += 1;
    let grant = g.clone();
    commit(&app, &mut controls, c).await?;
    Ok(json!({"grant":grant,"questions":[]}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub text: String,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub question_id: Option<String>,
    pub decision: Option<String>,
}
pub async fn accept_message(
    app: HostApplication,
    id: String,
    key: String,
    request: MessageInput,
) -> Result<(Outcome, Value), Error> {
    if request.text.trim().is_empty() || request.text.len() > 65_536 {
        return Err(Error::invalid());
    }
    if !request.resources.is_empty() || request.question_id.is_some() || request.decision.is_some()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let body = json!({"text":request.text,"resources":request.resources,"question_id":request.question_id,"decision":request.decision});
    let target = format!("/v1/companions/{id}/messages");
    let lookup = body.clone();
    let key_lookup = key.clone();
    let mut controls = app.0.controls.lock().await;
    if let Some(receipt) = app.0.repository.receipt(key_lookup, target, lookup).await? {
        return Ok(receipt);
    }
    let c = config(&app, &mut controls).await?;
    if c.companion.id != id {
        return Err(Error::new(ErrorKind::NotFound));
    }
    let assignment = c
        .companion
        .model_assignments
        .companion
        .as_ref()
        .ok_or(Error {
            kind: ErrorKind::StateConflict,
            reason: Some("model_unset"),
        })?;
    let p = rules::connection(&c, &assignment.connection_id)
        .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
    let secret = permitted(&app, &c, &controls, &p, "conversation")?;
    if controls.running.len() >= 8 {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let mut boundary = app.0.boundary.lock().await;
    let accepted = app.0.repository.accept(key, id, body).await?;
    if !accepted.created {
        return Ok((Outcome::Accepted, json!(accepted.receipt)));
    }
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&c.companion.id),
        json!(accepted.user),
    );
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&c.companion.id),
        json!(accepted.response),
    );
    let (cancel, receiver) = watch::channel(false);
    let response_id = accepted.receipt.response_message_id.clone();
    controls.running.insert(
        response_id.clone(),
        Running {
            connection: p.id.clone(),
            purpose: "conversation",
            cancel,
        },
    );
    tokio::spawn(generate(
        app.clone(),
        c,
        p,
        secret,
        request.text,
        response_id,
        receiver,
    ));
    Ok((Outcome::Accepted, json!(accepted.receipt)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub companion_id: String,
    pub purpose: String,
    pub model_id: Option<String>,
}
pub async fn check(
    app: HostApplication,
    id: String,
    key: String,
    request: CheckInput,
) -> Result<(Outcome, Value), Error> {
    use crate::ports::llm::{ChatMessage, Event, Generation};
    use futures_util::StreamExt;
    use std::time::Duration;
    if !["companion", "task"].contains(&request.purpose.as_str()) {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let body = json!({"companion_id":request.companion_id,"purpose":request.purpose,"model_id":request.model_id});
    let target = format!("/v1/connections/{id}/check");
    let mut controls = app.0.controls.lock().await;
    if let Some((outcome, response)) = app
        .0
        .repository
        .receipt(key.clone(), target.clone(), body.clone())
        .await?
    {
        if outcome == Outcome::Pending {
            return Err(Error::new(ErrorKind::RequestInProgress));
        }
        return Ok((outcome, response));
    }
    let c = config(&app, &mut controls).await?;
    if c.companion.id != request.companion_id {
        return Err(Error::new(ErrorKind::NotFound));
    }
    let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let secret = permitted(&app, &c, &controls, &p, "connection_check")?;
    if controls.running.len() >= 8 {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let assignment = if request.purpose == "task" {
        &c.companion.model_assignments.task
    } else {
        &c.companion.model_assignments.companion
    };
    let model = request
        .model_id
        .or_else(|| {
            assignment
                .as_ref()
                .filter(|a| a.connection_id == id)
                .map(|a| a.model_id.clone())
        })
        .or_else(|| {
            app.providers()
                .into_iter()
                .find(|d| d.id == p.adapter)
                .and_then(|d| d.models.first().map(|m| m.id.clone()))
        })
        .ok_or_else(Error::invalid)?;
    if model.trim().is_empty() || model.len() > 512 {
        return Err(Error::invalid());
    }
    let request_id = Uuid::new_v4().to_string();
    app.0
        .repository
        .reserve(Receipt {
            key: key.clone(),
            target: target.clone(),
            request: body.clone(),
            outcome: Outcome::Pending,
            response: json!({"request_id":request_id}),
        })
        .await?;
    let (cancel, mut cancelled) = watch::channel(false);
    controls.running.insert(
        request_id.clone(),
        Running {
            connection: id.clone(),
            purpose: "connection_check",
            cancel,
        },
    );
    drop(controls);
    let generation = Generation {
        endpoint: p.endpoint.clone(),
        options: p.options.clone(),
        model: model.clone(),
        preamble: String::new(),
        messages: vec![ChatMessage {
            role: "user".into(),
            text: "日本語で接続確認に成功したことを一文だけ返してください。".into(),
        }],
        max_tokens: 128,
    };
    let input = invocation(
        &app,
        p,
        secret,
        request_id.clone(),
        "connection_check",
        generation,
        cancelled.clone(),
    );
    let work = async {
        let mut stream = app.0.providers.generate(input).await?;
        let mut text = String::new();
        let mut chunks = 0;
        let mut finished = false;
        while let Some(event) = stream.next().await {
            match event? {
                Event::Delta(delta) => {
                    text.push_str(&delta);
                    chunks += 1;
                    if text.len() > 8192 {
                        return Err(Error::new(ErrorKind::ExternalFailure));
                    }
                }
                Event::Finished(reason) => {
                    finished = reason == "stop";
                    break;
                }
            }
        }
        if !finished || text.trim().is_empty() {
            return Err(Error::new(ErrorKind::ExternalFailure));
        }
        Ok((chunks, text.chars().count()))
    };
    let mut shutdown = app.0.shutdown.subscribe();
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err(Error::unavailable())
    } else {
        tokio::select! {biased;_=cancelled.changed()=>Err(Error::unavailable()),_=shutdown.changed()=>Err(Error::unavailable()),r=tokio::time::timeout(Duration::from_secs(90),work)=>r.unwrap_or_else(|_|Err(Error::unavailable()))}
    };
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let cancelled_now = controls
        .running
        .get(&request_id)
        .is_none_or(|r| *r.cancel.borrow());
    controls.running.remove(&request_id);
    let (outcome, response) = match result {
        Ok((chunks, scalars)) if !cancelled_now => {
            if let Some(mut p) = rules::connection(&c, &id) {
                p.capabilities = Some(json!({"streaming_text":true}));
                p.last_check = Some(
                    json!({"success":true,"model_id":model,"checked_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).map_err(|_|Error::unavailable())?}),
                );
                update_connection(&mut c, p);
            }
            (
                Outcome::Completed,
                json!({"request_id":request_id,"capabilities":{"streaming_text":true},"success":true,"model_id":model,"content_chunks":chunks,"unicode_scalars":scalars}),
            )
        }
        _ => (
            Outcome::Unavailable,
            json!({"request_id":request_id,"error":{"code":"unavailable","message":"接続確認を完了できませんでした","details":{}}}),
        ),
    };
    refresh(&app, &mut c);
    let mut boundary = app.0.boundary.lock().await;
    app.0
        .repository
        .complete_check(
            c.clone(),
            Receipt {
                key,
                target,
                request: body,
                outcome,
                response: response.clone(),
            },
        )
        .await?;
    publish(
        &mut boundary,
        &app,
        "settings.updated",
        None,
        json!(c.settings),
    );
    controls.config = Some(c);
    Ok((outcome, response))
}
impl HostApplication {
    pub fn providers(&self) -> Vec<crate::ports::llm::Descriptor> {
        self.0.providers.descriptors()
    }
    pub async fn reload_providers(&self) -> Result<(), Error> {
        let mut controls = self.0.controls.lock().await;
        for run in controls.running.values() {
            run.cancel.send_replace(true);
        }
        self.0.providers.reload().await?;
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
    pub async fn models(&self, id: String) -> Result<Vec<crate::ports::llm::Model>, Error> {
        let mut controls = self.0.controls.lock().await;
        let c = config(self, &mut controls).await?;
        let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let secret = permitted_data(self, &c, &controls, &p, "connection_check", &["metadata"])?;
        if !allowed(self, &c, &p, "connection_check", &["metadata"]) {
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
                connection: id,
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
            "connection_check",
            request,
            cancelled.clone(),
        );
        let mut shutdown = self.shutdown();
        let result = tokio::select! {biased;_=shutdown.changed()=>Err(Error::unavailable()),_=cancelled.changed()=>Err(Error::unavailable()),r=tokio::time::timeout(std::time::Duration::from_secs(30),self.0.providers.models(input))=>r.unwrap_or_else(|_|Err(Error::unavailable()))};
        self.0.controls.lock().await.running.remove(&job);
        result
    }
}
