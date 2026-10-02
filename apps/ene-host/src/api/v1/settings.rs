use crate::api::{
    error::{ApiError, wire},
    state::App,
};
use axum::{Json, Router, extract::State, routing::get};
use ene_protocol::host::Settings;
pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(settings).patch(patch_settings))
}
async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(wire(app.application.settings().await?)?))
}

async fn patch_settings(
    State(app): State<App>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_protocol::host::SettingsPatch,
    >,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        ene_core::application::operations::patch_settings(
            app.application,
            crate::api::error::command(input)?,
        )
        .await?,
    ))
}
