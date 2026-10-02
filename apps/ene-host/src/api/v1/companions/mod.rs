mod messages;

use crate::{
    api::extractors::ApiJson,
    app::App,
    conversation as db,
    error::{ApiError, Error},
    service::{MODEL, commit, config},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use ene_protocol::host::Companion;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(companions))
        .route("/{companion_id}", get(companion).patch(patch_companion))
        .nest("/{companion_id}/messages", messages::router())
}

async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(vec![
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .companion,
    ]))
}
async fn companion(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Companion>, ApiError> {
    Ok(Json(
        app.store
            .companion(&id)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompanionPatch {
    revision: u64,
    name: Option<String>,
    mind: Option<String>,
    model_assignments: Option<Value>,
}
async fn patch_companion(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<CompanionPatch>,
) -> Result<Json<Value>, Error> {
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if c.companion.id != id {
        return Err(Error::new(StatusCode::NOT_FOUND, "not_found"));
    }
    if c.companion.revision != request.revision {
        return Err(Error::new(StatusCode::CONFLICT, "revision_conflict"));
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
        let object = assignments.as_object().ok_or_else(Error::invalid)?;
        for (key, value) in object {
            if !["companion", "task"].contains(&key.as_str()) {
                return Err(Error::invalid());
            }
            if !value.is_null() {
                let map = value.as_object().ok_or_else(Error::invalid)?;
                if map.len() != 2
                    || value["model_id"] != MODEL
                    || value["connection_id"]
                        .as_str()
                        .and_then(|id| db::connection(&c, id))
                        .is_none()
                {
                    return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
                }
            }
            let assignment = if value.is_null() {
                None
            } else {
                Some(value.clone())
            };
            if key == "companion" {
                c.companion.model_assignments.companion = assignment;
            } else {
                c.companion.model_assignments.task = assignment;
            }
        }
    }
    c.companion.revision += 1;
    db::refresh(&mut c);
    commit(&app, &mut controls, c.clone()).await?;
    Ok(Json(json!(c.companion)))
}
