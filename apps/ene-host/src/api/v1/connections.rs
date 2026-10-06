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
        .routes(routes!(availability))
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
    Ok(response(app.application.check(id, key, input).await?))
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
    Ok(Json(app.application.models(id, query.companion_id).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AvailabilityQuery {
    companion_id: String,
}
#[utoipa::path(get, path = "/{connection_id}/availability", tag = "connections",
    params(("connection_id" = String, Path), ("companion_id" = String, Query)),
    responses((status = 200, description = "Effective authorization without external requests", body = ene_api_types::host::ConnectionAvailability))
)]
async fn availability(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    ApiQuery(query): ApiQuery<AvailabilityQuery>,
) -> Result<Json<ene_api_types::host::ConnectionAvailability>, ApiError> {
    Ok(Json(
        app.application.availability(id, query.companion_id).await?,
    ))
}
