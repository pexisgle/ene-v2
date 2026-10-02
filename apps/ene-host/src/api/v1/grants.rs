use crate::{
    api::extractors::{ApiJson, ApiQuery, IdempotencyKey},
    app::App,
    conversation::{self as db, Grant},
    error::{ApiError, Error},
    service::{cancel_purpose, commit, config, publish},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(grants).post(post_grant))
        .route("/{grant_id}", patch(patch_grant).delete(delete_grant))
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
    let values = app
        .store
        .run(|db| Ok(json!(crate::conversation::load(db)?.grants)))
        .await
        .map_err(|_| ApiError::unavailable())?;
    Ok(Json(
        serde_json::from_value(values).map_err(|_| ApiError::unavailable())?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantInput {
    companion_id: String,
    kind: String,
    connection_id: String,
    destinations: Vec<String>,
    data_kinds: Vec<String>,
    purpose: String,
}
async fn post_grant(
    State(app): State<App>,
    IdempotencyKey(key): IdempotencyKey,
    ApiJson(request): ApiJson<GrantInput>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let body = json!({"companion_id":request.companion_id,"kind":request.kind,"connection_id":request.connection_id,"destinations":request.destinations,"data_kinds":request.data_kinds,"purpose":request.purpose});
    let key_lookup = key.clone();
    let lookup = body.clone();
    let mut controls = app.controls.lock().await;
    if let Some((status, value)) = app
        .store
        .run(move |conn| db::receipt(conn, &key_lookup, "/v1/grants", &lookup))
        .await
        .map_err(Error::from_store)?
    {
        return Ok((
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Json(value),
        ));
    }
    let mut c = config(&app, &mut controls).await?;
    if request.companion_id != c.companion.id {
        return Err(Error::new(StatusCode::NOT_FOUND, "not_found"));
    }
    let connection = db::connection(&c, &request.connection_id)
        .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
    if request.kind != "data_transfer"
        || !["conversation", "connection_check"].contains(&request.purpose.as_str())
        || request
            .destinations
            .iter()
            .any(|d| !["https://openrouter.ai", "openai"].contains(&d.as_str()))
        || request
            .data_kinds
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        || request.destinations.is_empty()
        || request.data_kinds.is_empty()
    {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
        ));
    }
    let grant = Grant {
        id: Uuid::new_v4().to_string(),
        companion_id: request.companion_id,
        kind: request.kind,
        connection_id: request.connection_id,
        destinations: request.destinations,
        data_kinds: request.data_kinds,
        purpose: request.purpose,
        status: "active".into(),
        revision: 1,
        invalidation_reason: None,
        connection_fingerprint: db::fingerprint(&connection),
    };
    c.grants.push(grant.clone());
    db::refresh(&mut c);
    let response = json!({"grant":grant,"questions":[]});
    let saved = c.clone();
    let saved_response = response.clone();
    let mut boundary = app.boundary.lock().await;
    app.store
        .run(move |conn| {
            let tx = conn.transaction().map_err(crate::infrastructure::sqlite::Error::from)?;
            db::write_configuration(&tx, &saved)?;
            db::save_receipt(&tx, &key, "/v1/grants", &body, 201, &saved_response)?;
            tx.commit().map_err(crate::infrastructure::sqlite::Error::from)
        })
        .await
        .map_err(Error::from_store)?;
    controls.config = Some(c.clone());
    publish(&mut boundary, &app, "grants.updated", None, json!(c.grants));
    publish(
        &mut boundary,
        &app,
        "companion.updated",
        Some(&c.companion.id),
        json!(c.companion),
    );
    Ok((StatusCode::CREATED, Json(response)))
}
async fn delete_grant(State(app): State<App>, Path(id): Path<String>) -> Result<StatusCode, Error> {
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if let Some(g) = c.grants.iter_mut().find(|g| g.id == id)
        && g.status == "active"
    {
        cancel_purpose(&controls, &g.connection_id, &g.purpose);
        g.status = "invalidated".into();
        g.revision += 1;
        g.invalidation_reason = Some("revoked".into());
        commit(&app, &mut controls, c).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantPatch {
    revision: u64,
    destinations: Option<Vec<String>>,
    data_kinds: Option<Vec<String>>,
}
async fn patch_grant(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<GrantPatch>,
) -> Result<Json<Value>, Error> {
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let g = c
        .grants
        .iter_mut()
        .find(|g| g.id == id)
        .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
    if g.revision != request.revision {
        return Err(Error::new(StatusCode::CONFLICT, "revision_conflict"));
    }
    if g.status != "active" {
        return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
    }
    if let Some(destinations) = request.destinations {
        if destinations
            .iter()
            .any(|d| !["https://openrouter.ai", "openai"].contains(&d.as_str()))
        {
            return Err(Error::invalid());
        }
        g.destinations = destinations;
    }
    if let Some(data) = request.data_kinds {
        if data
            .iter()
            .any(|d| !["conversation", "metadata", "test_input"].contains(&d.as_str()))
        {
            return Err(Error::invalid());
        }
        g.data_kinds = data;
    }
    cancel_purpose(&controls, &g.connection_id, &g.purpose);
    g.revision += 1;
    let grant = g.clone();
    commit(&app, &mut controls, c).await?;
    Ok(Json(json!({"grant":grant,"questions":[]})))
}
