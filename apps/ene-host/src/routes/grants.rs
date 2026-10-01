use super::companions::companion;
use crate::{app::App, error::ApiError};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::Value;

pub(super) fn router() -> Router<App> {
    Router::new().route("/grants", get(grants))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantQuery {
    companion_id: Option<String>,
}
async fn grants(
    State(app): State<App>,
    query: Result<Query<GrantQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    if let Some(c) = query.companion_id {
        let _ = companion(State(app), Path(c)).await?;
    }
    Ok(Json(vec![]))
}
