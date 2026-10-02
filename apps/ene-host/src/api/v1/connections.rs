use crate::api::{
    error::{ApiError, response, wire},
    extractors::{ApiJson, ApiQuery, IdempotencyKey},
    state::App,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::Value;
pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/{connection_id}/check", post(check))
        .route("/{connection_id}/models", get(models))
}
async fn check(
    State(app): State<App>,
    Path(id): Path<String>,
    IdempotencyKey(key): IdempotencyKey,
    ApiJson(input): ApiJson<ene_protocol::host::CheckInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok(response(
        tokio::spawn(ene_core::application::operations::check(
            app.application,
            id,
            key,
            crate::api::error::command(input)?,
        ))
        .await
        .map_err(|_| ApiError::unavailable())??,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsQuery {
    companion_id: Option<String>,
}
async fn models(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ModelsQuery>,
) -> Result<Json<Vec<ene_protocol::provider::Model>>, ApiError> {
    if let Some(c) = query.companion_id {
        app.application.companion(&c).await?;
    }
    Ok(Json(wire(app.application.models(id).await?)?))
}
