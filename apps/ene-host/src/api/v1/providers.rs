use crate::api::{
    error::{ApiError, wire},
    extractors::ApiJson,
    state::App,
};
use axum::{Json, extract::State};
use ene_api_types::provider::ProviderInfo;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(reload))
        .routes(routes!(enable))
}
#[utoipa::path(get, path = "/", tag = "providers",
    responses((status = 200, description = "Success", body = [ene_api_types::provider::ProviderInfo]))
)]
async fn list(State(app): State<App>) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    Ok(Json(wire(app.application.providers())?))
}
#[utoipa::path(post, path = "/reload", tag = "providers",
    responses((status = 200, description = "Success", body = [ene_api_types::provider::ProviderInfo]))
)]
async fn reload(State(app): State<App>) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    app.application.reload_providers().await?;
    Ok(Json(wire(app.application.providers())?))
}
#[utoipa::path(put, path = "/{provider_id}/enabled", tag = "providers",
    params(("provider_id" = String, Path)),
    request_body = ene_api_types::ProviderEnabled,
    responses((status = 200, description = "Success", body = [ene_api_types::provider::ProviderInfo]))
)]
async fn enable(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    ApiJson(input): ApiJson<ene_api_types::ProviderEnabled>,
) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    app.application.enable_provider(id, input.enabled).await?;
    Ok(Json(wire(app.application.providers())?))
}
