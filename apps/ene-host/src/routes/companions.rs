use crate::{app::App, error::ApiError};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use ene_shared::host::Companion;

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/companions", get(companions))
        .route("/companions/{companion_id}", get(companion))
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
pub(super) async fn companion(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Companion>, ApiError> {
    let c = app
        .store
        .snapshot()
        .await
        .map_err(|_| ApiError::unavailable())?
        .companion;
    if c.id != id {
        return Err(ApiError::not_found());
    }
    Ok(Json(c))
}
