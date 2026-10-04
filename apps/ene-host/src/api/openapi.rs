//! OpenAPI is assembled from the same resource registrations as the HTTP router.
use crate::api::{state::App, v1};
use axum::Json;
use utoipa::{
    OpenApi,
    openapi::{
        self,
        security::{Http, HttpAuthScheme, SecurityRequirement, SecurityScheme},
    },
};
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "ene host API",
        version = "1.0.0",
        description = "Authenticated API v1. The current host listens on loopback. No implicit mutation retries. Subscribe to events before obtaining a snapshot; notification offsets count Unicode scalars. Provider options are adapter-specific non-secret JSON."
    ),
    components(schemas(
        ene_api_types::ApiErrorBody,
        ene_api_types::EventReady,
        ene_api_types::Notification,
        ene_api_types::MessageDelta,
        ene_api_types::ModelAssignment
    ))
)]
struct Contract;

pub(super) fn documented() -> OpenApiRouter<App> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .nest("/v1", v1::router().routes(routes!(document)))
}
pub(crate) fn specification() -> openapi::OpenApi {
    let (_, mut specification) = documented().split_for_parts();
    let components = specification
        .components
        .as_mut()
        .expect("contract components");
    components.add_security_scheme(
        "bearerAuth",
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
    specification.security = Some(vec![SecurityRequirement::new(
        "bearerAuth",
        Vec::<String>::new(),
    )]);
    let failure = openapi::response::ResponseBuilder::new()
        .description("API failure. Inspect error.code and request_id; do not retry mutations with a new key.")
        .content("application/json", openapi::ContentBuilder::new()
            .schema(Some(openapi::Ref::from_schema_name("ApiErrorBody"))).build()).build();
    for path in specification.paths.paths.values_mut() {
        for operation in [
            &mut path.get,
            &mut path.post,
            &mut path.put,
            &mut path.patch,
            &mut path.delete,
        ]
        .into_iter()
        .flatten()
        {
            for status in ["400", "401", "403", "404", "409", "422", "503"] {
                operation
                    .responses
                    .responses
                    .entry(status.into())
                    .or_insert_with(|| failure.clone().into());
            }
        }
    }
    // OpenAPI describes the upgrade; explicitly link the subsequent frames and
    // sequence contract so other clients can implement the same synchronization.
    specification.extensions = Some(serde_json::from_value(serde_json::json!({
        "x-ene-events": {
            "ready": {"$ref": "#/components/schemas/EventReady"},
            "notification": {"$ref": "#/components/schemas/Notification"},
            "message.delta.data": {"$ref": "#/components/schemas/MessageDelta"},
            "resync_required": {"type": "object", "properties": {"type": {"const": "resync_required"}}, "required": ["type"]},
            "synchronization": "Subscribe -> ready.connection_id -> GET snapshot?connection_id -> discard seq <= snapshot.cursor.seq -> ordered deltas. Resubscribe on epoch change, sequence gap, offset mismatch or resync_required. Unknown notification types still consume seq. message.delta.offset counts Unicode scalars."
        }
    })).expect("OpenAPI extension"));
    specification
}
#[utoipa::path(get, path = "/openapi.json", tag = "contract",
    responses((status = 200, description = "OpenAPI 3.1 document", body = Object)))]
async fn document() -> Json<openapi::OpenApi> {
    Json(specification())
}
