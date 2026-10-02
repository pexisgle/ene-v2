use crate::{
    api::extractors::{ApiJson, IdempotencyKey},
    app::App,
    conversation as db,
    error::Error,
    service::{MODEL, Running, config, publish, update_connection},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use futures_util::StreamExt;
use rig_core::{
    completion::CompletionRequest,
    streaming::{Item, StreamEvent},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

pub(super) fn router() -> Router<App> {
    Router::new().route("/{connection_id}/check", post(check_connection))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    companion_id: String,
    purpose: String,
    model_id: Option<String>,
}
async fn check_connection(
    State(app): State<App>,
    Path(id): Path<String>,
    IdempotencyKey(key): IdempotencyKey,
    ApiJson(request): ApiJson<CheckInput>,
) -> Result<(StatusCode, Json<Value>), Error> {
    tokio::spawn(check(app, id, key, request))
        .await
        .map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))?
}
async fn check(
    app: App,
    id: String,
    key: String,
    request: CheckInput,
) -> Result<(StatusCode, Json<Value>), Error> {
    if !["companion", "task"].contains(&request.purpose.as_str())
        || request.model_id.as_deref().is_some_and(|m| m != MODEL)
    {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
        ));
    }
    let body = json!({"companion_id":request.companion_id,"purpose":request.purpose,"model_id":request.model_id});
    let target = format!("/v1/connections/{id}/check");
    let key_lookup = key.clone();
    let target_lookup = target.clone();
    let request_lookup = body.clone();
    let mut controls = app.controls.lock().await;
    if let Some((status, value)) = app
        .store
        .run(move |conn| db::receipt(conn, &key_lookup, &target_lookup, &request_lookup))
        .await
        .map_err(Error::from_store)?
    {
        if status == 0 {
            return Err(Error::new(StatusCode::CONFLICT, "request_in_progress"));
        }
        return Ok((
            StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(value),
        ));
    }
    let c = config(&app, &mut controls).await?;
    if c.companion.id != request.companion_id {
        return Err(Error::new(StatusCode::NOT_FOUND, "not_found"));
    }
    let p =
        db::connection(&c, &id).ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
    if !db::allowed(&c, &p, "connection_check", &["test_input"]) {
        return Err(Error::new(StatusCode::FORBIDDEN, "permission_required"));
    }
    let secret = controls
        .secrets
        .get(&id)
        .ok_or(Error {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "unavailable",
            reason: Some("credential_missing"),
        })?
        .value
        .clone();
    let request_id = Uuid::new_v4().to_string();
    let saved_id = request_id.clone();
    let saved_key = key.clone();
    let saved_target = target.clone();
    app.store
        .run(move |conn| {
            db::save_receipt(
                conn,
                &saved_key,
                &saved_target,
                &body,
                0,
                &json!({"request_id":saved_id}),
            )
        })
        .await
        .map_err(Error::from_store)?;
    let (cancel, mut cancelled) = watch::channel(false);
    controls.running.insert(
        request_id.clone(),
        Running {
            connection: id.clone(),
            purpose: "connection_check",
            cancel,
        },
    );
    drop(controls);
    let work = async {
        let client = rig_core::providers::openrouter::new(secret);
        let request =
            CompletionRequest::new("日本語で接続確認に成功したことを一文だけ返してください。")
                .max_tokens(128)
                .additional_params(json!({"provider":{"only":["openai"],"allow_fallbacks":false}}));
        let mut stream = client.chat(MODEL).stream(request).map_err(|_| ())?;
        let mut text = String::new();
        let mut chunks = 0;
        while let Some(event) = stream.next().await {
            if let Item::Event(StreamEvent::Text { text: delta, .. }) = event.map_err(|_| ())? {
                text.push_str(&delta);
                chunks += 1;
            }
            if text.len() > 8192 {
                return Err(());
            }
        }
        stream.finish().await.map_err(|_| ())?;
        if text.trim().is_empty() {
            return Err(());
        }
        Ok::<_, ()>((chunks, text.chars().count()))
    };
    let mut shutdown = app.shutdown.subscribe();
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err(())
    } else {
        tokio::select! {biased;_=cancelled.changed()=>Err(()),_=shutdown.changed()=>Err(()),result=tokio::time::timeout(Duration::from_secs(90),work)=>result.unwrap_or(Err(()))}
    };
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    controls.running.remove(&request_id);
    let (status, response) = match result {
        Ok((chunks, scalars)) => {
            if let Some(mut p) = db::connection(&c, &id) {
                p.capabilities = Some(json!({"streaming_text":true}));
                p.last_check =
                    Some(json!({"success":true,"model_id":MODEL,"checked_at":db::now()}));
                update_connection(&mut c, p);
            }
            (
                StatusCode::OK,
                json!({"request_id":request_id,"capabilities":{"streaming_text":true},"success":true,"model_id":MODEL,"content_chunks":chunks,"unicode_scalars":scalars}),
            )
        }
        Err(()) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"request_id":request_id,"error":{"code":"unavailable","message":"接続確認を完了できませんでした","details":{}}}),
        ),
    };
    db::refresh(&mut c);
    let saved_config = c.clone();
    let mut boundary = app.boundary.lock().await;
    let saved_response = response.clone();
    let saved_status = status.as_u16();
    app.store
        .run(move |conn| {
            let tx = conn.transaction()?;
            db::write_configuration(&tx, &saved_config)?;
            tx.execute(
                "UPDATE receipts SET status=?1,response=?2 WHERE key=?3 AND target=?4",
                tokio_rusqlite::rusqlite::params![
                    saved_status,
                    saved_response.to_string(),
                    key,
                    target
                ],
            )
            .map_err(crate::infrastructure::sqlite::Error::from)?;
            tx.commit().map_err(crate::infrastructure::sqlite::Error::from)
        })
        .await
        .map_err(Error::from_store)?;
    publish(
        &mut boundary,
        &app,
        "settings.updated",
        None,
        json!(c.settings),
    );
    controls.config = Some(c);
    Ok((status, Json(response)))
}
