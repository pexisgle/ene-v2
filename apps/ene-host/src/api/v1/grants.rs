use crate::api::{
    error::{ApiError, response, wire},
    extractors::ApiQuery,
    state::App,
};
use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use serde_json::Value;
pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(grants).post(post_grant))
        .route(
            "/{grant_id}",
            axum::routing::patch(patch_grant).delete(delete_grant),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantQuery {
    companion_id: Option<String>,
}
async fn grants(
    State(app): State<App>,
    ApiQuery(query): ApiQuery<GrantQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    Ok(Json(wire(
        app.application
            .grants(query.companion_id.as_deref())
            .await?,
    )?))
}

async fn post_grant(
    State(app): State<App>,
    crate::api::extractors::IdempotencyKey(key): crate::api::extractors::IdempotencyKey,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_protocol::host::GrantInput,
    >,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    Ok(response(
        ene_core::application::operations::post_grant(
            app.application,
            key,
            crate::api::error::command(input)?,
        )
        .await?,
    ))
}
async fn patch_grant(
    State(app): State<App>,
    axum::extract::Path(id): axum::extract::Path<String>,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_protocol::host::GrantPatch,
    >,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        ene_core::application::operations::patch_grant(
            app.application,
            id,
            crate::api::error::command(input)?,
        )
        .await?,
    ))
}
async fn delete_grant(
    State(app): State<App>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    ene_core::application::operations::delete_grant(app.application, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
