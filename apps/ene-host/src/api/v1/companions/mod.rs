mod messages;
use crate::api::{
    error::{ApiError, wire},
    state::App,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use ene_protocol::host::Companion;
pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(companions))
        .route("/{companion_id}", get(companion).patch(patch_companion))
        .nest("/{companion_id}/messages", messages::router())
}
async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(wire(app.application.companions().await?)?))
}
async fn companion(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Companion>, ApiError> {
    Ok(Json(wire(app.application.companion(&id).await?)?))
}

async fn patch_companion(
    State(app): State<App>,
    Path(id): Path<String>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_protocol::host::CompanionPatch,
    >,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        ene_core::application::operations::patch_companion(
            app.application,
            id,
            crate::api::error::command(input)?,
        )
        .await?,
    ))
}
