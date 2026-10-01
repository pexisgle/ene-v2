//! Route files mirror the /v1 resource paths; domain and storage logic live outside this module.
mod companions;
mod events;
mod grants;
mod messages;
mod settings;
mod snapshot;
#[cfg(test)]
mod tests;

use crate::{app::App, auth::authenticate, error::ApiError};
use axum::{Router, http::StatusCode, middleware};

pub fn router(app: App) -> Router {
    let v1 = Router::new()
        .merge(snapshot::router())
        .merge(companions::router())
        .merge(messages::router())
        .merge(settings::router())
        .merge(grants::router())
        .merge(events::router());
    Router::new()
        .nest("/v1", v1)
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "未対応のAPIです")
        })
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .with_state(app)
}
