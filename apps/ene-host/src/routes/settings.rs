use crate::{app::App, error::ApiError};
use axum::{Json, Router, extract::State, routing::get};
use ene_shared::host::Settings;

pub(super) fn router() -> Router<App> {
    Router::new().route("/settings", get(settings))
}

async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .settings,
    ))
}
