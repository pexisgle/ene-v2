mod companions;
mod events;
mod grants;
mod settings;
mod snapshot;

use crate::app::App;
use axum::Router;

pub(super) fn router() -> Router<App> {
    Router::new()
        .nest("/companions", companions::router())
        .nest("/events", events::router())
        .nest("/grants", grants::router())
        .nest("/settings", settings::router())
        .nest("/snapshot", snapshot::router())
}
