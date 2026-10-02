use utoipa_axum::{router::OpenApiRouter, routes};
mod messages;
use crate::api::{error::ApiError, state::App};
use axum::{Json, extract::State};
use ene_api_types::host::Companion;
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(companions))
        .routes(routes!(companion, patch_companion))
        .nest("/{companion_id}/messages", messages::router())
}
#[utoipa::path(get, path = "/", tag = "companions",
    responses((status = 200, description = "Success", body = [ene_api_types::Companion]))
)]
async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(app.application.companions().await?))
}
#[utoipa::path(get, path = "/{companion_id}", tag = "companions",
    params(("companion_id" = String, Path)),
    responses((status = 200, description = "Success", body = ene_api_types::Companion))
)]
async fn companion(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
) -> Result<Json<Companion>, ApiError> {
    Ok(Json(app.application.companion(&id).await?))
}

#[utoipa::path(patch, path = "/{companion_id}", tag = "companions",
    params(("companion_id" = String, Path)),
    request_body = ene_api_types::CompanionPatch,
    responses((status = 200, description = "Success", body = ene_api_types::Companion))
)]
async fn patch_companion(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_api_types::host::CompanionPatch,
    >,
) -> Result<Json<ene_api_types::Companion>, ApiError> {
    Ok(Json(
        ene_core::application::operations::patch_companion(app.application, id, input).await?,
    ))
}
