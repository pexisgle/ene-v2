use crate::api::{error::ApiError, state::App};
use axum::{Json, extract::State};
use ene_api_types::host::Settings;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new().routes(routes!(settings, patch_settings))
}
#[utoipa::path(get, path = "/", tag = "settings",
    responses((status = 200, description = "Success", body = ene_api_types::Settings))
)]
async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(app.application.settings().await?))
}

#[utoipa::path(patch, path = "/", tag = "settings",
    request_body = ene_api_types::SettingsPatch,
    responses((status = 200, description = "Success", body = ene_api_types::Settings))
)]
async fn patch_settings(
    State(app): State<App>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_api_types::host::SettingsPatch,
    >,
) -> Result<Json<ene_api_types::Settings>, ApiError> {
    Ok(Json(
        app.mutate(ene_core::application::operations::patch_settings(
            app.application.clone(),
            input,
        ))
        .await?,
    ))
}
