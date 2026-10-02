use crate::api::{
    error::{ApiError, wire},
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
        .route("/", get(messages))
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
