use crate::api::{
    error::{ApiError, response},
    extractors::ApiQuery,
    state::App,
};
use axum::{Json, extract::State};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(grants, post_grant))
        .routes(routes!(patch_grant, delete_grant))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantQuery {
    companion_id: Option<String>,
}
#[utoipa::path(get, path = "/", tag = "grants",
    params(("companion_id" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = [ene_api_types::DataTransferGrant]))
)]
async fn grants(
    State(app): State<App>,
    ApiQuery(query): ApiQuery<GrantQuery>,
) -> Result<Json<Vec<ene_api_types::DataTransferGrant>>, ApiError> {
    Ok(Json(
        app.application
            .grants(query.companion_id.as_deref())
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(post, path = "/", tag = "grants",
    params(("Idempotency-Key" = String, Header, min_length = 1, max_length = 200)),
    request_body = ene_api_types::GrantInput,
    responses((status = 201, description = "Success", body = ene_api_types::GrantResult))
)]
async fn post_grant(
    State(app): State<App>,
    crate::api::extractors::IdempotencyKey(key): crate::api::extractors::IdempotencyKey,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_api_types::host::GrantInput,
    >,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    Ok(response(
        app.mutate(ene_core::application::operations::post_grant(
            app.application.clone(),
            key,
            input,
        ))
        .await?,
    ))
}
#[utoipa::path(patch, path = "/{grant_id}", tag = "grants",
    params(("grant_id" = String, Path)),
    request_body = ene_api_types::GrantPatch,
    responses((status = 200, description = "Success", body = ene_api_types::GrantResult))
)]
async fn patch_grant(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_api_types::host::GrantPatch,
    >,
) -> Result<Json<ene_api_types::GrantResult>, ApiError> {
    Ok(Json(
        app.mutate(ene_core::application::operations::patch_grant(
            app.application.clone(),
            id,
            input,
        ))
        .await?,
    ))
}
#[utoipa::path(delete, path = "/{grant_id}", tag = "grants",
    params(("grant_id" = String, Path)),
    responses((status = 204, description = "Success"))
)]
async fn delete_grant(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    app.mutate(ene_core::application::operations::delete_grant(
        app.application.clone(),
        id,
    ))
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
