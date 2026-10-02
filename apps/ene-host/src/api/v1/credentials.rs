use crate::api::{error::ApiError, extractors::ApiJson, state::App};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::put,
};
use serde::Deserialize;
use serde_json::Value;
pub(super) fn router() -> Router<App> {
    Router::new().route("/{connection_id}", put(register).delete(revoke))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    secret: Option<String>,
    source: Option<String>,
}
async fn register(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<Input>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        tokio::spawn(async move {
            app.application
                .register_credentials(id, input.secret, input.source)
                .await
        })
        .await
        .map_err(|_| ApiError::unavailable())??,
    ))
}
async fn revoke(State(app): State<App>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    app.application.revoke_credentials(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
