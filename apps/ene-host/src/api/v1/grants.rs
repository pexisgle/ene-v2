use crate::api::{
    error::{ApiError, wire},
    extractors::ApiQuery,
    state::App,
};
use axum::{Json, extract::State};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new().routes(routes!(grants))
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
    Ok(Json(wire(
        app.application
            .grants(query.companion_id.as_deref())
            .await?,
    )?))
}
