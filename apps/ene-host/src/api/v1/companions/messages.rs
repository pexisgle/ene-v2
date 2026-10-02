use crate::api::{
    error::{ApiError, response, wire},
    extractors::ApiQuery,
    state::App,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use ene_protocol::host::{Message, Page};
use serde::Deserialize;
pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(messages).post(post_message))
        .route("/{message_id}", get(message))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}
async fn messages(
    State(app): State<App>,
    Path(c): Path<String>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<Page<Message>>, ApiError> {
    Ok(Json(wire(
        app.application
            .messages(c, query.cursor, query.limit.unwrap_or(50))
            .await?,
    )?))
}
async fn message(
    State(app): State<App>,
    Path((c, id)): Path<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(wire(app.application.message(c, id).await?)?))
}

async fn post_message(
    State(app): State<App>,
    Path(id): Path<String>,
    crate::api::extractors::IdempotencyKey(key): crate::api::extractors::IdempotencyKey,
    crate::api::extractors::ApiJson(input): crate::api::extractors::ApiJson<
        ene_protocol::host::MessageInput,
    >,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    let value = tokio::spawn(ene_core::application::operations::accept_message(
        app.application,
        id,
        key,
        crate::api::error::command(input)?,
    ))
    .await
    .map_err(|_| ApiError::unavailable())??;
    Ok(response(value))
}
