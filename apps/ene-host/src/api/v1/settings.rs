use crate::api::{
    error::{ApiError, wire},
    state::App,
};
use axum::{Json, Router, extract::State, routing::get};
use ene_protocol::host::Settings;
pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(settings))
}
async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(wire(app.application.settings().await?)?))
}
