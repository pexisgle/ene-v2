use crate::{app::App, error::ApiError};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use ene_shared::host::Snapshot;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn router() -> Router<App> {
    Router::new().route("/snapshot", get(snapshot))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    connection_id: Option<String>,
}
async fn snapshot(
    State(app): State<App>,
    query: Result<Query<SnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Snapshot>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let boundary = app.boundary.lock().await;
    if query
        .connection_id
        .as_ref()
        .is_some_and(|c| !boundary.connections.contains(c))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "resync_required",
            "通知購読からやり直してください",
        ));
    }
    let stored = app
        .store
        .snapshot()
        .await
        .map_err(|_| ApiError::unavailable())?;
    let message_cursors = BTreeMap::from([(stored.companion.id.clone(), stored.next_cursor)]);
    Ok(Json(Snapshot {
        host_id: stored.host_id,
        environment_id: stored.environment_id,
        capabilities: json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications"],"adapters":[],"model_reference_formats":[]}),
        cursor: boundary.cursor.clone(),
        companions: vec![stored.companion],
        messages: stored.messages,
        message_cursors,
        tasks: vec![],
        questions: vec![],
        settings: stored.settings,
        grants: vec![],
    }))
}
