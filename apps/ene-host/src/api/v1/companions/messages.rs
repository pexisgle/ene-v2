use crate::api::{
    error::{ApiError, wire},
    extractors::ApiQuery,
    state::App,
};
use axum::{Json, extract::State};
use ene_api_types::host::{Message, Page};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .routes(routes!(messages))
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
    Ok(Json(wire(
        app.application
            .messages(c, query.cursor, query.limit.unwrap_or(50))
            .await?,
    )?))
}
#[utoipa::path(get, path = "/{message_id}", tag = "companions",
    params(("companion_id" = String, Path), ("message_id" = String, Path)),
    responses((status = 200, description = "Success", body = ene_api_types::Message))
)]
async fn message(
    State(app): State<App>,
    crate::api::extractors::ApiPath((c, id)): crate::api::extractors::ApiPath<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(wire(app.application.message(c, id).await?)?))
}
