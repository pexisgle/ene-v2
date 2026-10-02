use crate::{
    api::extractors::ApiJson,
    app::App,
    conversation::{self as db, ProviderConnection, ProviderOptions},
    error::{ApiError, Error},
    service::{ENDPOINT, active, commit, config, forget_credential},
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use ene_protocol::host::Settings;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(settings).patch(patch_settings))
}

async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .settings,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionInput {
    id: Option<String>,
    kind: String,
    adapter: String,
    endpoint: String,
    options: ProviderOptions,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    revision: u64,
    connections: Vec<ConnectionInput>,
}
async fn patch_settings(
    State(app): State<App>,
    ApiJson(request): ApiJson<SettingsPatch>,
) -> Result<Json<Value>, Error> {
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if request.revision != c.settings.revision {
        return Err(Error::new(StatusCode::CONFLICT, "revision_conflict"));
    }
    let mut connections = vec![];
    let mut ids = std::collections::HashSet::new();
    for input in request.connections {
        if input.kind != "llm"
            || input.adapter != "openrouter"
            || input.endpoint != ENDPOINT
            || input.options.only != ["openai"]
            || input.options.allow_fallbacks
        {
            return Err(Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_capability",
            ));
        }
        let previous = input
            .id
            .as_ref()
            .map(|id| {
                db::connection(&c, id).ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))
            })
            .transpose()?;
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
            && db::fingerprint(&old) != db::fingerprint(&p)
        {
            if active(&controls, &old.id) {
                return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
            }
            for g in c
                .grants
                .iter_mut()
                .filter(|g| g.connection_id == old.id && g.status == "active")
            {
                g.status = "invalidated".into();
                g.invalidation_reason = Some("connection_changed".into());
                g.revision += 1;
            }
            p.capabilities = None;
            p.last_check = None;
        }
        connections.push(json!(p));
    }
    for old in &c.settings.connections {
        let id = old["id"].as_str().ok_or_else(Error::invalid)?;
        if !ids.contains(id)
            && (active(&controls, id)
                || [
                    &c.companion.model_assignments.companion,
                    &c.companion.model_assignments.task,
                ]
                .iter()
                .any(|a| a.as_ref().is_some_and(|a| a["connection_id"] == id))
                || c.grants
                    .iter()
                    .any(|g| g.connection_id == id && g.status == "active"))
        {
            return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
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
        forget_credential(reference);
    }
    Ok(Json(json!(c.settings)))
}
