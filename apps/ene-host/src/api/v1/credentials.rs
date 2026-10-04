use crate::api::{error::ApiError, extractors::ApiJson, state::App};
use axum::{Json, extract::State, http::StatusCode};
use utoipa_axum::{router::OpenApiRouter, routes};
pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new().routes(routes!(register, revoke))
}
#[utoipa::path(put, path = "/{connection_id}", tag = "credentials",
    params(("connection_id" = String, Path)),
    request_body = ene_api_types::CredentialInput,
    responses((status = 200, description = "Success", body = ene_api_types::CredentialResult))
)]
async fn register(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
    ApiJson(input): ApiJson<ene_api_types::CredentialInput>,
) -> Result<Json<ene_api_types::CredentialResult>, ApiError> {
    let application = app.application.clone();
    Ok(Json(
        app.mutate(async move {
            application
                .register_credentials(id, input.secret, input.source)
                .await
        })
        .await?,
    ))
}
#[utoipa::path(delete, path = "/{connection_id}", tag = "credentials",
    params(("connection_id" = String, Path)),
    responses((status = 204, description = "Success"))
)]
async fn revoke(
    State(app): State<App>,
    crate::api::extractors::ApiPath(id): crate::api::extractors::ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    let application = app.application.clone();
    app.mutate(async move { application.revoke_credentials(id).await })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
