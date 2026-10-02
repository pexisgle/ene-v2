use crate::api::{error::ApiError, state::App};
use axum::{Json, extract::State};
use ene_api_types::host::Settings;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new().routes(routes!(settings))
}
#[utoipa::path(get, path = "/", tag = "settings",
    responses((status = 200, description = "Success", body = ene_api_types::Settings))
)]
async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(app.application.settings().await?))
}
