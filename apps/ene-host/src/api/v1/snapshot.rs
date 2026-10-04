use crate::{api::error::ApiError, api::extractors::ApiQuery, api::state::App};
use axum::{Json, extract::State};
use ene_api_types::host::Snapshot;
use serde::Deserialize;
use serde_json::json;
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
    let message_cursors = stored.message_cursors;
    Ok(Json(Snapshot {
        host_id: stored.host_id,
        environment_id: stored.environment_id,
        capabilities: json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications","chat","connection_settings","credentials","data_transfer_grants","connection_check","provider_plugins"],"adapters":app.application.providers().iter().filter(|p|p.state=="ready").map(|p|p.id.clone()).collect::<Vec<_>>(),"model_reference_formats":["provider_model_id"]}),
        cursor: view.cursor,
        companions: stored.companions,
        messages: stored.messages,
        message_cursors,
        tasks: vec![],
        questions: vec![],
        settings: stored.settings,
        grants: view.grants.into_iter().map(Into::into).collect(),
    }))
}
