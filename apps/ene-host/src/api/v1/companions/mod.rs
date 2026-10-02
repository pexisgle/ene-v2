use utoipa_axum::{router::OpenApiRouter, routes};
mod messages;
use crate::api::{
    error::{ApiError, wire},
    state::App,
};
use axum::{Json, extract::State};
use ene_api_types::host::Companion;
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(companions))
        .routes(routes!(companion))
        .nest("/{companion_id}/messages", messages::router())
}
#[utoipa::path(get, path = "/", tag = "companions",
    responses((status = 200, description = "Success", body = [ene_api_types::Companion]))
)]
async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(wire(app.application.companions().await?)?))
}
#[utoipa::path(get, path = "/{companion_id}", tag = "companions",
    params(("companion_id" = String, Path)),
    responses((status = 200, description = "Success", body = ene_api_types::Companion))
)]
async fn companion(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
) -> Result<Json<Companion>, ApiError> {
    Ok(Json(wire(app.application.companion(&id).await?)?))
}
