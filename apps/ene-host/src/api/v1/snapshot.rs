use crate::{api::error::ApiError, api::extractors::ApiQuery, api::state::App};
use axum::{Json, extract::State};
use ene_api_types::host::Snapshot;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new().routes(routes!(snapshot))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    connection_id: Option<String>,
}
#[utoipa::path(get, path = "/", tag = "snapshot",
    params(("connection_id" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = ene_api_types::Snapshot))
)]
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
    let mut snapshot = Snapshot {
        host_id: stored.host_id,
        environment_id: stored.environment_id,
        capabilities: json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications"],"adapters":[],"model_reference_formats":[]}),
        cursor: view.cursor,
        companions: vec![stored.companion],
        messages: stored.messages,
        message_cursors,
        tasks: vec![],
        questions: vec![],
        settings: stored.settings,
        grants: vec![],
        health: ene_api_types::host::HostHealth {
            ready: true,
            stopping: *app.application.shutdown().borrow(),
            ..Default::default()
        },
    };
    snapshot
        .bound_response()
        .map_err(|_| ene_core::domain::Error::new(ene_core::domain::ErrorKind::ResourceLimit))?;
    Ok(Json(snapshot))
}
