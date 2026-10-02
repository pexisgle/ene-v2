mod messages;

use crate::{app::App, error::ApiError};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use ene_shared::host::Companion;

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(companions))
        .route("/{companion_id}", get(companion))
        .nest("/{companion_id}/messages", messages::router())
}

async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(vec![
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .companion,
    ]))
}
async fn companion(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Companion>, ApiError> {
    Ok(Json(
        app.store
            .companion(&id)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?,
    ))
}
