use super::companions::companion;
use crate::store;
use crate::{app::App, error::ApiError};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use ene_shared::host::{Message, Page};
use serde::Deserialize;

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/companions/{companion_id}/messages", get(messages))
        .route(
            "/companions/{companion_id}/messages/{message_id}",
            get(message),
        )
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
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Page<Message>>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let _ = companion(State(app.clone()), Path(c.clone())).await?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit)
        || query
            .cursor
            .as_ref()
            .is_some_and(|cursor| store::parse_cursor(&c, cursor).is_err())
    {
        return Err(ApiError::invalid());
    }
    Ok(Json(
        app.store
            .messages(c, query.cursor, limit)
            .await
            .map_err(|_| ApiError::unavailable())?,
    ))
}
async fn message(
    State(app): State<App>,
    Path((c, id)): Path<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(
        app.store
            .message(c, id)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?,
    ))
}
