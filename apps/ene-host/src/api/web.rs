//! Same-origin HttpOnly session authentication for future web clients.
use super::{auth, error::ApiError, state::App};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
pub(super) fn router(app: App) -> Router {
    Router::new()
        .route("/session", post(login).delete(logout))
        .with_state(app)
}
async fn login(State(app): State<App>, headers: HeaderMap) -> Result<Response, ApiError> {
    if !auth::same_origin(&app, &headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "同じホストから接続してください",
        ));
    }
    if !auth::owner(&app, &headers) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "認証が必要です",
        ));
    }
    if let Some(old) = auth::session_id(&headers) {
        app.sessions.revoke(old).await;
    }
    let session = app.sessions.create().await?;
    let secure = if headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    Ok((
        StatusCode::NO_CONTENT,
        [
            (
                header::SET_COOKIE,
                format!(
                    "ene_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=28800{secure}",
                    session.id
                ),
            ),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response())
}
async fn logout(State(app): State<App>, headers: HeaderMap) -> Result<Response, ApiError> {
    if !auth::same_origin(&app, &headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "同じホストから接続してください",
        ));
    }
    if let Some(id) = auth::session_id(&headers) {
        app.sessions.revoke(id).await;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [
            (
                header::SET_COOKIE,
                "ene_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response())
}
