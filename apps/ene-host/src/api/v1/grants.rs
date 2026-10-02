use crate::api::extractors::ApiQuery;
use crate::{app::App, error::ApiError};
use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use serde_json::Value;

pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(grants))
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
    if let Some(c) = query.companion_id {
        app.store
            .companion(&c)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?;
    }
    Ok(Json(vec![]))
}
