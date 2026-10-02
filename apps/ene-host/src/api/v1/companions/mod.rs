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
        .route("/{companion_id}", get(companion))
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
