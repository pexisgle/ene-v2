use crate::{
    api::error::{ApiError, wire},
    api::extractors::ApiQuery,
    api::state::App,
};
use axum::{Json, Router, extract::State, routing::get};
use ene_protocol::host::Snapshot;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(snapshot))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    connection_id: Option<String>,
}
async fn snapshot(
    State(app): State<App>,
    ApiQuery(query): ApiQuery<SnapshotQuery>,
) -> Result<Json<Snapshot>, ApiError> {
    let view = app
        .application
        .snapshot(query.connection_id.as_deref())
        .await?;
    let stored = view.stored;
    let message_cursors = BTreeMap::from([(stored.companion.id.clone(), stored.next_cursor)]);
    Ok(Json(Snapshot {
        host_id: stored.host_id,
        environment_id: stored.environment_id,
        capabilities: json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications"],"adapters":[],"model_reference_formats":[]}),
        cursor: wire(view.cursor)?,
        companions: vec![wire(stored.companion)?],
        messages: wire(stored.messages)?,
        message_cursors,
        tasks: vec![],
        questions: vec![],
        settings: wire(stored.settings)?,
        grants: vec![],
    }))
}
