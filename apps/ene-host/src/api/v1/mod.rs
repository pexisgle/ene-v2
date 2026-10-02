mod companions;
mod events;
mod grants;
mod settings;
mod snapshot;

use crate::api::state::App;
use utoipa_axum::router::OpenApiRouter;

pub(super) fn router() -> OpenApiRouter<App> {
    OpenApiRouter::new()
        .nest("/companions", companions::router())
        .nest("/events", events::router())
        .nest("/grants", grants::router())
        .nest("/settings", settings::router())
        .nest("/snapshot", snapshot::router())
}
