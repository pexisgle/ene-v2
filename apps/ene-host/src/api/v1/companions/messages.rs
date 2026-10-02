use crate::{
    api::extractors::{ApiJson, ApiQuery, IdempotencyKey},
    app::App,
    conversation as db,
    error::{ApiError, Error},
    service::{Running, config, generate, permitted, publish},
    store,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use ene_protocol::host::{Message, Page};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

pub(super) fn router() -> Router<App> {
    Router::new()
        .route("/", get(messages).post(post_message))
        .route("/{message_id}", get(message))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}
async fn messages(
    State(app): State<App>,
    Path(c): Path<String>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<Page<Message>>, ApiError> {
    app.store
        .companion(&c)
        .await
        .map_err(|_| ApiError::unavailable())?
        .ok_or_else(ApiError::not_found)?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit)
        || query
            .cursor
            .as_ref()
            .is_some_and(|cursor| store::parse_cursor(&c, cursor).is_err())
    {
        return Err(ApiError::invalid());
    }
    Ok(Json(
        app.store
            .messages(c, query.cursor, limit)
            .await
            .map_err(|_| ApiError::unavailable())?,
    ))
}
async fn message(
    State(app): State<App>,
    Path((c, id)): Path<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(
        app.store
            .message(c, id)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageInput {
    text: String,
    #[serde(default)]
    resources: Vec<Value>,
    question_id: Option<String>,
    decision: Option<String>,
}
async fn post_message(
    State(app): State<App>,
    Path(id): Path<String>,
    IdempotencyKey(key): IdempotencyKey,
    ApiJson(request): ApiJson<MessageInput>,
) -> Result<(StatusCode, Json<Value>), Error> {
    tokio::spawn(accept_message(app, id, key, request))
        .await
        .map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))?
}
async fn accept_message(
    app: App,
    id: String,
    key: String,
    request: MessageInput,
) -> Result<(StatusCode, Json<Value>), Error> {
    if request.text.trim().is_empty() || request.text.len() > 65_536 {
        return Err(Error::invalid());
    }
    if !request.resources.is_empty() || request.question_id.is_some() || request.decision.is_some()
    {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
        ));
    }
    let body = json!({"text":request.text,"resources":request.resources,"question_id":request.question_id,"decision":request.decision});
    let target = format!("/v1/companions/{id}/messages");
    let lookup = body.clone();
    let key_lookup = key.clone();
    let mut controls = app.controls.lock().await;
    if let Some((status, value)) = app
        .store
        .run(move |conn| db::receipt(conn, &key_lookup, &target, &lookup))
        .await
        .map_err(Error::from_store)?
    {
        return Ok((
            StatusCode::from_u16(status).unwrap_or(StatusCode::ACCEPTED),
            Json(value),
        ));
    }
    let c = config(&app, &mut controls).await?;
    if c.companion.id != id {
        return Err(Error::new(StatusCode::NOT_FOUND, "not_found"));
    }
    let (p, secret) = permitted(&c, &controls, "conversation")?;
    if controls.running.len() >= 8 {
        return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
    }
    let mut boundary = app.boundary.lock().await;
    let accepted = app
        .store
        .run(move |conn| db::accept(conn, &key, &id, body))
        .await
        .map_err(Error::from_store)?;
    if !accepted.created {
        return Ok((StatusCode::ACCEPTED, Json(json!(accepted.receipt))));
    }
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&c.companion.id),
        json!(accepted.user),
    );
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&c.companion.id),
        json!(accepted.response),
    );
    let (cancel, receiver) = watch::channel(false);
    let response_id = accepted.receipt.response_message_id.clone();
    controls.running.insert(
        response_id.clone(),
        Running {
            connection: p.id,
            purpose: "conversation",
            cancel,
        },
    );
    tokio::spawn(generate(
        app.clone(),
        c,
        secret,
        request.text,
        response_id,
        receiver,
    ));
    Ok((StatusCode::ACCEPTED, Json(json!(accepted.receipt))))
}
