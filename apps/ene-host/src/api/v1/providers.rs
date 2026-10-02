use crate::api::{
    error::{ApiError, wire},
    extractors::ApiJson,
    state::App,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post, put},
};
use ene_protocol::provider::ProviderInfo;
use serde::Deserialize;
pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(list))
        .route("/reload", post(reload))
        .route("/{provider_id}/enabled", put(enable))
}
async fn list(State(app): State<App>) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    Ok(Json(wire(app.application.providers())?))
}
async fn reload(State(app): State<App>) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    app.application.reload_providers().await?;
    Ok(Json(wire(app.application.providers())?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {
    enabled: bool,
}
async fn enable(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<Enable>,
) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    app.application.enable_provider(id, input.enabled).await?;
    Ok(Json(wire(app.application.providers())?))
}
