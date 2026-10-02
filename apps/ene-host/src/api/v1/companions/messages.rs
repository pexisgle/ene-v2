use crate::api::{
    error::{ApiError, response},
    extractors::ApiQuery,
    state::App,
};
use axum::{Json, extract::State};
use ene_api_types::host::{Message, Page};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(messages, post_message))
        .routes(routes!(message))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}
#[utoipa::path(get, path = "/", tag = "companions",
    params(("companion_id" = String, Path), ("cursor" = Option<String>, Query), ("limit" = Option<usize>, Query)),
    responses((status = 200, description = "Success", body = ene_api_types::Page<ene_api_types::Message>))
)]
async fn messages(
    State(app): State<App>,
    crate::api::extractors::ApiPath(c): crate::api::extractors::ApiPath<String>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<Page<Message>>, ApiError> {
    Ok(Json(
        app.application
            .messages(c, query.cursor, query.limit.unwrap_or(50))
            .await?,
    ))
}
#[utoipa::path(get, path = "/{message_id}", tag = "companions",
    params(("companion_id" = String, Path), ("message_id" = String, Path)),
    responses((status = 200, description = "Success", body = ene_api_types::Message))
)]
async fn message(
    State(app): State<App>,
    crate::api::extractors::ApiPath((c, id)): crate::api::extractors::ApiPath<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(app.application.message(c, id).await?))
}

#[utoipa::path(post, path = "/", tag = "companions",
    params(("companion_id" = String, Path), ("Idempotency-Key" = String, Header, min_length = 1, max_length = 200)),
    request_body = ene_api_types::MessageInput,
    responses((status = 202, description = "Success", body = ene_api_types::MessageAcceptance))
)]
async fn post_message(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    crate::api::extractors::IdempotencyKey(key): crate::api::extractors::IdempotencyKey,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_api_types::host::MessageInput,
    >,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    let value = tokio::spawn(ene_core::application::operations::accept_message(
        app.application,
        id,
        key,
        input,
    ))
    .await
    .map_err(|_| ApiError::unavailable())??;
    Ok(response(value))
}
