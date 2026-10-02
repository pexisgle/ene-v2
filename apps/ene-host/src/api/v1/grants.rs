use crate::api::{
    error::{ApiError, wire},
    extractors::ApiQuery,
    state::App,
};
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
    Ok(Json(wire(
        app.application
            .grants(query.companion_id.as_deref())
            .await?,
    )?))
}
