use crate::api::{
    error::{ApiError, response},
    extractors::{ApiJson, ApiQuery, IdempotencyKey},
    state::App,
};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(check))
        .routes(routes!(models))
}
#[utoipa::path(post, path = "/{connection_id}/check", tag = "connections",
    params(("connection_id" = String, Path), ("Idempotency-Key" = String, Header, min_length = 1, max_length = 200)),
    request_body = ene_api_types::CheckInput,
    responses((status = 200, description = "Success", body = ene_api_types::CheckResult))
)]
async fn check(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    IdempotencyKey(key): IdempotencyKey,
    ApiJson(input): ApiJson<ene_api_types::host::CheckInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok(response(
        tokio::spawn(ene_core::application::operations::check(
            app.application,
            id,
            key,
            input,
        ))
        .await
        .map_err(|_| ApiError::unavailable())??,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsQuery {
    companion_id: Option<String>,
}
#[utoipa::path(get, path = "/{connection_id}/models", tag = "connections",
    params(("connection_id" = String, Path), ("companion_id" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = [ene_api_types::provider::Model]))
)]
async fn models(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    ApiQuery(query): ApiQuery<ModelsQuery>,
) -> Result<Json<Vec<ene_api_types::provider::Model>>, ApiError> {
    if let Some(c) = query.companion_id {
        app.application.companion(&c).await?;
    }
    Ok(Json(app.application.models(id).await?))
}
