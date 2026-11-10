//! Connections, provider credentials, consent grants and operation availability.
use super::{
    Host, Runtime,
    db::{
        Database, Receipt, ReceiptStatus, Result, aborted, digest, document, invalid, new_id,
        next_revision, not_found, now, permission, validate_id, validate_key,
    },
    mind::companion,
    model, revoke_and_stop,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConnectionRecord {
    pub(crate) connection: w::Connection,
    pub(crate) credential_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GrantRecord {
    pub(crate) grant: w::Grant,
    pub(crate) consent: w::Consent,
}

impl GrantRecord {
    pub(crate) fn permits(
        &self,
        companion_id: &str,
        connection_id: &str,
        purpose: &str,
        destination: &str,
        fingerprint: &Value,
        settings_revision: u64,
    ) -> bool {
        self.grant.companion_id == companion_id
            && self.grant.connection_id == connection_id
            && self.grant.purpose == purpose
            && self.grant.status == "active"
            && self.consent.settings_revision == settings_revision
            && self.grant.destinations.iter().any(|v| v == destination)
            && model::Purpose::from_str(purpose)
                .is_some_and(|purpose| covers(&self.grant.data_kinds, purpose))
            && self
                .grant
                .connection_fingerprint
                .as_option()
                .is_some_and(|doc| {
                    serde_json::from_str::<Value>(&doc.json).ok().as_ref() == Some(fingerprint)
                })
    }
}

/// Whether `kinds` covers every data kind `purpose` sends.
fn covers(kinds: &[String], purpose: model::Purpose) -> bool {
    purpose
        .required_data_kinds()
        .iter()
        .all(|required| kinds.iter().any(|kind| kind == required))
}

fn stored(database: &Database) -> Result<w::Settings> {
    Ok(database
        .get::<w::Settings>("settings")?
        .unwrap_or_else(|| w::Settings {
            revision: 1,
            ..Default::default()
        }))
}

/// The settings revision alone: a model binding is invalidated by it, and a
/// check that reads only the revision need not list every connection.
pub(crate) fn revision(database: &Database) -> Result<u64> {
    Ok(stored(database)?.revision)
}

pub(crate) fn settings(database: &Database) -> Result<w::Settings> {
    let mut current = stored(database)?;
    current.connections = database
        .list::<ConnectionRecord>("connection:")?
        .into_iter()
        .map(|(_, record)| record.connection)
        .collect();
    Ok(current)
}

/// Returns the settings and the credential references to forget.
pub(crate) fn update_settings(
    world: &mut World,
    request: w::UpdateSettingsRequest,
) -> Result<(w::Settings, Vec<String>)> {
    let (updated, forgotten) = {
        let database = &world.resource::<Host>().db;
        let current = settings(database)?;
        if current.revision != request.revision {
            return Err(aborted("settings revision changed"));
        }
        let Some(inputs) = request.connections.into_option() else {
            return Ok((current, Vec::new()));
        };
        let mut records = database.list::<ConnectionRecord>("connection:")?;
        let mut next = Vec::new();
        for input in inputs.items {
            let id = input.id.filter(|id| !id.is_empty()).unwrap_or_else(new_id);
            validate_id(&id)?;
            let adapter = input.adapter;
            if adapter.is_empty() {
                return Err(invalid("adapter is required"));
            }
            let kind = input.kind;
            let endpoint = input.endpoint.trim().trim_end_matches('/').to_owned();
            let options =
                serde_json::from_str::<Value>(&input.options.json).unwrap_or_else(|_| json!({}));
            model::validate_connection(&adapter, &kind, &endpoint, &options)?;
            let existing = records
                .iter()
                .position(|(_, record)| record.connection.id == id)
                .map(|index| records.remove(index).1);
            if input.create
                && let Some(existing) = &existing
                && (existing.connection.adapter != adapter
                    || existing.connection.kind != kind
                    || existing.connection.endpoint != endpoint)
            {
                return Err(aborted("connection already exists"));
            }
            let record = match existing {
                Some(mut record) => {
                    record.connection.adapter = adapter;
                    record.connection.kind = kind;
                    record.connection.endpoint = endpoint;
                    record.connection.options = document(options).into();
                    record
                }
                None => ConnectionRecord {
                    connection: w::Connection {
                        id: id.clone(),
                        kind,
                        adapter,
                        endpoint,
                        options: document(options).into(),
                        credential_state: "missing".into(),
                        enabled: true,
                        ..Default::default()
                    },
                    credential_ref: None,
                },
            };
            next.push((format!("connection:{id}"), record));
        }
        // What the request did not name is gone, with its credential and
        // the grants that let it send anything.
        let removed: Vec<ConnectionRecord> =
            records.into_iter().map(|(_, record)| record).collect();
        let forgotten: Vec<String> = removed
            .iter()
            .filter_map(|record| record.credential_ref.clone())
            .collect();
        let gone: Vec<String> = removed
            .into_iter()
            .map(|record| record.connection.id)
            .collect();
        let revision = next_revision(current.revision)?;
        database.transaction(|tx| {
            for (key, _) in tx.list::<ConnectionRecord>("connection:")? {
                tx.delete(&key)?;
            }
            for (key, record) in &next {
                tx.put(key, record)?;
            }
            for (key, mut record) in tx.list::<GrantRecord>("grant:")? {
                if !gone.contains(&record.grant.connection_id) || record.grant.status != "active" {
                    continue;
                }
                record.grant.status = "revoked".into();
                record.grant.revision = next_revision(record.grant.revision)?;
                tx.put(&key, &record)?;
            }
            tx.put(
                "settings",
                &w::Settings {
                    revision,
                    ..Default::default()
                },
            )
        })?;
        (settings(database)?, forgotten)
    };
    revoke_and_stop(world, None);
    world.resource_mut::<Host>().notify(
        None,
        None,
        w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
            settings: updated.clone().into(),
            ..Default::default()
        })),
    );
    Ok((updated, forgotten))
}

pub(crate) fn grants(database: &Database, companion: Option<&str>) -> Result<Vec<w::Grant>> {
    Ok(database
        .list::<GrantRecord>("grant:")?
        .into_iter()
        .filter(|(_, record)| companion.is_none_or(|id| record.grant.companion_id == id))
        .map(|(_, record)| record.grant)
        .collect())
}

pub(crate) fn create_grant(world: &mut World, request: w::CreateGrantRequest) -> Result<w::Grant> {
    let grant = {
        let database = &world.resource::<Host>().db;
        validate_key(&request.request_key)?;
        let _record = companion(database, &request.companion_id)?;
        let connection = database
            .get::<ConnectionRecord>(&format!("connection:{}", request.connection_id))?
            .ok_or_else(|| not_found("connection not found"))?;
        let provider = model::builtin(&connection.connection.adapter)
            .ok_or_else(|| aborted("unsupported provider adapter"))?;
        let origin = model::origin(&connection.connection.endpoint)
            .ok_or_else(|| invalid("connection endpoint is invalid"))?;
        if request.destinations.is_empty() || request.destinations.iter().any(|d| d != &origin) {
            return Err(invalid("grant destination is not the connection's"));
        }
        let purpose = model::Purpose::from_str(&request.purpose)
            .ok_or_else(|| invalid("invalid grant purpose or data kinds"))?;
        if !covers(&request.data_kinds, purpose) {
            return Err(invalid("invalid grant purpose or data kinds"));
        }
        let consent = request
            .consent
            .as_option()
            .cloned()
            .ok_or_else(|| invalid("consent is required"))?;
        let current = settings(database)?;
        if consent.settings_revision != current.revision
            || consent.provider_revision != provider.revision
        {
            return Err(aborted("consent is stale"));
        }
        let scope = format!("grant:{}", request.companion_id);
        let hash = digest(
            serde_json::to_string(&request)
                .map_err(|_| invalid("invalid grant"))?
                .as_bytes(),
        );
        if let Some(response) = database.check_receipt(&scope, &request.request_key, &hash)? {
            return serde_json::from_value(response).map_err(|_| invalid("invalid stored grant"));
        }
        let fingerprint = model::fingerprint(provider, &connection.connection);
        let grant = w::Grant {
            id: new_id(),
            companion_id: request.companion_id.clone(),
            connection_id: request.connection_id,
            destinations: request.destinations,
            data_kinds: request.data_kinds,
            purpose: request.purpose,
            status: "active".into(),
            revision: 1,
            connection_fingerprint: document(fingerprint).into(),
            ..Default::default()
        };
        let receipt = Receipt {
            hash,
            status: ReceiptStatus::Accepted,
            response: serde_json::to_value(&grant).map_err(|_| invalid("invalid grant"))?,
            updated_at: now(),
        };
        database.transaction(|tx| {
            tx.put(
                &format!("grant:{}", grant.id),
                &GrantRecord {
                    grant: grant.clone(),
                    consent,
                },
            )?;
            tx.put(
                &format!("receipt:{scope}:{}", request.request_key),
                &receipt,
            )
        })?;
        grant
    };
    notify_grants(world)?;
    Ok(grant)
}

pub(crate) fn update_grant(world: &mut World, request: w::UpdateGrantRequest) -> Result<w::Grant> {
    let grant = {
        let database = &world.resource::<Host>().db;
        let key = format!("grant:{}", request.id);
        let mut record = database
            .get::<GrantRecord>(&key)?
            .ok_or_else(|| not_found("grant not found"))?;
        if record.grant.revision != request.revision {
            return Err(aborted("grant revision changed"));
        }
        if record.grant.status != "active" {
            return Err(aborted("grant is not active"));
        }
        let connection = database
            .get::<ConnectionRecord>(&format!("connection:{}", record.grant.connection_id))?
            .ok_or_else(|| not_found("connection not found"))?;
        let origin = model::origin(&connection.connection.endpoint)
            .ok_or_else(|| invalid("connection endpoint is invalid"))?;
        if let Some(list) = request.destinations.as_option() {
            if list.values.is_empty() || list.values.iter().any(|d| d != &origin) {
                return Err(invalid("grant destination is not the connection's"));
            }
            record.grant.destinations.clone_from(&list.values);
        }
        if let Some(list) = request.data_kinds.as_option() {
            let purpose = model::Purpose::from_str(&record.grant.purpose)
                .ok_or_else(|| invalid("invalid grant data kinds"))?;
            if !covers(&list.values, purpose) {
                return Err(invalid("invalid grant data kinds"));
            }
            record.grant.data_kinds.clone_from(&list.values);
        }
        record.grant.revision = next_revision(request.revision)?;
        database.put(&key, &record)?;
        record.grant
    };
    notify_grants(world)?;
    Ok(grant)
}

pub(crate) fn revoke_grant(world: &mut World, id: &str) -> Result<()> {
    {
        let database = &world.resource::<Host>().db;
        let key = format!("grant:{id}");
        let mut record = database
            .get::<GrantRecord>(&key)?
            .ok_or_else(|| not_found("grant not found"))?;
        if record.grant.status != "revoked" {
            record.grant.status = "revoked".into();
            record.grant.revision = next_revision(record.grant.revision)?;
            database.put(&key, &record)?;
        }
    }
    revoke_and_stop(world, None);
    notify_grants(world)
}

fn notify_grants(world: &mut World) -> Result<()> {
    let items = grants(&world.resource::<Host>().db, None)?;
    world.resource_mut::<Host>().notify(
        None,
        None,
        w::notification::Event::GrantsUpdated(Box::new(w::GrantsUpdated {
            items,
            ..Default::default()
        })),
    );
    Ok(())
}

pub(crate) fn availability(
    database: &Database,
    companion_id: &str,
    connection_id: &str,
) -> Result<w::Availability> {
    let companion = companion(database, companion_id)?;
    let connection = database
        .get::<ConnectionRecord>(&format!("connection:{connection_id}"))?
        .ok_or_else(|| not_found("connection not found"))?;
    let provider = model::builtin(&connection.connection.adapter)
        .ok_or_else(|| aborted("unsupported provider adapter"))?;
    let destination = model::origin(&connection.connection.endpoint).unwrap_or_default();
    let fingerprint = model::fingerprint(provider, &connection.connection);
    let current = settings(database)?;
    let all_grants = database.list::<GrantRecord>("grant:")?;
    let operations = model::Purpose::ALL
        .into_iter()
        .map(|purpose| {
            let op = purpose.name();
            let allowed = all_grants.iter().any(|(_, r)| {
                r.permits(
                    companion_id,
                    connection_id,
                    op,
                    &destination,
                    &fingerprint,
                    current.revision,
                )
            });
            w::OperationAvailability {
                operation: op.into(),
                purpose: op.into(),
                data_kinds: purpose
                    .required_data_kinds()
                    .iter()
                    .map(|kind| (*kind).into())
                    .collect(),
                authorization: if allowed { "allowed" } else { "missing" }.into(),
                dependency: "connection".into(),
                busy: false,
                runnable: allowed && connection.connection.enabled,
                reason: (!allowed).then(|| "permission_required".into()),
                ..Default::default()
            }
        })
        .collect();
    Ok(w::Availability {
        companion_id: companion.companion.id,
        connection_id: connection_id.into(),
        settings_revision: current.revision,
        provider_revision: Some(provider.revision.into()),
        destinations: vec![destination],
        connection_fingerprint: document(fingerprint).into(),
        operations,
        ..Default::default()
    })
}

pub(crate) async fn register_credentials(
    runtime: &Runtime,
    id: String,
    secret: Option<String>,
) -> Result<w::CredentialResult> {
    let _ = get_connection(runtime, &id).await?;
    let secret = secret
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("secret is required"))?;
    let reference = format!("connection:{id}");
    runtime.credentials().save(&reference, &secret).await?;
    let saved = runtime
        .call({
            let reference = reference.clone();
            move |app| {
                set_credential(
                    &app.world().resource::<Host>().db,
                    &id,
                    Some(reference),
                    "registered",
                )?;
                revoke_and_stop(app.world_mut(), None);
                Ok(())
            }
        })
        .await;
    if saved.is_err() {
        let _ = runtime.credentials().forget(&reference).await;
    }
    saved?;
    Ok(w::CredentialResult {
        credential_state: "registered".into(),
        ..Default::default()
    })
}

pub(crate) async fn revoke_credentials(runtime: &Runtime, id: String) -> Result<w::Empty> {
    let cleanup = runtime
        .credentials()
        .forget(&format!("connection:{id}"))
        .await;
    runtime
        .call(move |app| {
            set_credential(&app.world().resource::<Host>().db, &id, None, "missing")?;
            revoke_and_stop(app.world_mut(), None);
            Ok(())
        })
        .await?;
    cleanup?;
    Ok(w::Empty::default())
}

pub(crate) async fn check_connection(
    runtime: &Runtime,
    id: String,
    request_key: String,
) -> Result<w::CheckResult> {
    let connection = get_connection(runtime, &id).await?;
    let secret = model::secret(&runtime.credentials(), &connection)
        .ok_or_else(|| permission("credential_missing"))?;
    let (models, capabilities) = model::probe(&connection, &secret).await?;
    runtime
        .call({
            let capabilities = capabilities.clone();
            move |app| {
                let key = format!("connection:{id}");
                let db = &app.world().resource::<Host>().db;
                let mut record = db
                    .get::<ConnectionRecord>(&key)?
                    .ok_or_else(|| not_found("connection not found"))?;
                record.connection.last_check = document(capabilities).into();
                db.put(&key, &record)
            }
        })
        .await?;
    Ok(w::CheckResult {
        request_id: request_key,
        capabilities: document(capabilities).into(),
        success: true,
        model_id: models.first().map_or_else(String::new, |m| m.id.clone()),
        content_chunks: models.len() as u64,
        ..Default::default()
    })
}

pub(crate) async fn list_models(runtime: &Runtime, id: String) -> Result<w::Models> {
    let connection = get_connection(runtime, &id).await?;
    let secret = model::secret(&runtime.credentials(), &connection)
        .ok_or_else(|| permission("credential_missing"))?;
    let (items, _) = model::probe(&connection, &secret).await?;
    Ok(w::Models {
        items,
        ..Default::default()
    })
}

async fn get_connection(runtime: &Runtime, id: &str) -> Result<ConnectionRecord> {
    let id = id.to_owned();
    runtime
        .call(move |app| {
            app.world()
                .resource::<Host>()
                .db
                .get::<ConnectionRecord>(&format!("connection:{id}"))?
                .ok_or_else(|| not_found("connection not found"))
        })
        .await
}

fn set_credential(
    database: &Database,
    connection: &str,
    reference: Option<String>,
    state: &str,
) -> Result<()> {
    let key = format!("connection:{connection}");
    let mut record = database
        .get::<ConnectionRecord>(&key)?
        .ok_or_else(|| not_found("connection not found"))?;
    record.credential_ref = reference;
    record.connection.credential_state = state.into();
    database.put(&key, &record)
}
