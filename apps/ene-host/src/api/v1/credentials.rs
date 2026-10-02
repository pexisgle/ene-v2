use crate::{
    api::extractors::ApiJson,
    app::App,
    conversation as db,
    error::Error,
    service::{
        Secret, active, cancel_connection, commit, config, forget_credential, update_connection,
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::put,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub(super) fn router() -> Router<App> {
    Router::new().route(
        "/{connection_id}",
        put(put_credentials).delete(delete_credentials),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    secret: Option<String>,
    source: Option<String>,
}
async fn put_credentials(
    State(app): State<App>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<Credentials>,
) -> Result<Json<Value>, Error> {
    tokio::spawn(register(app, id, request))
        .await
        .map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))?
}
async fn register(app: App, id: String, request: Credentials) -> Result<Json<Value>, Error> {
    let value = match (request.secret, request.source.as_deref()) {
        (Some(value), None) => value,
        (None, Some("environment")) => std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE, "credential_missing"))?,
        _ => return Err(Error::invalid()),
    };
    if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
        return Err(Error::invalid());
    }
    let nonce = Uuid::new_v4().to_string();
    let reference = Uuid::new_v4().to_string();
    let previous = {
        let mut controls = app.controls.lock().await;
        let mut c = config(&app, &mut controls).await?;
        let mut p = db::connection(&c, &id)
            .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
        if controls
            .secrets
            .get(&id)
            .is_some_and(|secret| secret.value == value)
        {
            return Ok(Json(json!({"credential_state":p.credential_state})));
        }
        if active(&controls, &id) {
            return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
        }
        p.credential_state = "session_only".into();
        let previous = c.credential_refs.remove(&id);
        update_connection(&mut c, p);
        commit(&app, &mut controls, c).await?;
        controls.secrets.insert(
            id.clone(),
            Secret {
                value: value.clone(),
                nonce: nonce.clone(),
            },
        );
        previous
    };
    if let Some(reference) = previous {
        forget_credential(reference);
    }
    if request.source.as_deref() == Some("environment")
        || std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some()
    {
        return Ok(Json(json!({"credential_state":"session_only"})));
    }
    let saved_reference = reference.clone();
    let abandoned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cleanup = abandoned.clone();
    let job = tokio::task::spawn_blocking(move || {
        keyring::Entry::new("ene-v2", &saved_reference).and_then(|entry| {
            let result = entry.set_password(&value);
            if cleanup.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = entry.delete_credential();
            }
            result
        })
    });
    let saved = matches!(
        tokio::time::timeout(Duration::from_secs(5), job).await,
        Ok(Ok(Ok(())))
    );
    if !saved {
        abandoned.store(true, std::sync::atomic::Ordering::SeqCst);
        forget_credential(reference.clone());
    }
    let mut controls = app.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    if !controls.secrets.get(&id).is_some_and(|s| s.nonce == nonce) {
        forget_credential(reference);
        return Err(Error::new(StatusCode::CONFLICT, "state_conflict"));
    }
    if saved {
        let mut p = db::connection(&c, &id)
            .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
        p.credential_state = "stored".into();
        c.credential_refs.insert(id, reference.clone());
        update_connection(&mut c, p);
        if let Err(error) = commit(&app, &mut controls, c).await {
            forget_credential(reference);
            return Err(error);
        }
    }
    Ok(Json(
        json!({"credential_state":if saved{"stored"}else{"session_only"}}),
    ))
}
async fn delete_credentials(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    let reference = {
        let mut controls = app.controls.lock().await;
        let mut c = config(&app, &mut controls).await?;
        let mut p = db::connection(&c, &id)
            .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "not_found"))?;
        cancel_connection(&controls, &id);
        controls.secrets.remove(&id);
        p.credential_state = "missing".into();
        let reference = c.credential_refs.remove(&id);
        update_connection(&mut c, p);
        commit(&app, &mut controls, c).await?;
        reference
    };
    if let Some(reference) = reference {
        forget_credential(reference);
    }
    Ok(StatusCode::NO_CONTENT)
}
