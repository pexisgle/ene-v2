use crate::api::error::Error;
use crate::{
    app::{App, EventBoundary},
    conversation::{self as db, Configuration, ProviderConnection},
};
use axum::http::StatusCode;
use ene_protocol::host::{Failure, MessageStatus, Notification, Role};
use futures_util::StreamExt;
use rig_core::{
    completion::CompletionRequest,
    streaming::{Item, StreamEvent},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;

pub const MODEL: &str = "openai/gpt-6-luna";
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1";

pub struct Secret {
    pub value: String,
    pub nonce: String,
}
pub struct Running {
    pub connection: String,
    pub purpose: &'static str,
    pub cancel: watch::Sender<bool>,
}
#[derive(Default)]
pub struct Controls {
    pub config: Option<Configuration>,
    pub secrets: BTreeMap<String, Secret>,
    pub running: BTreeMap<String, Running>,
}

fn failure(code: &'static str) -> Failure {
    Failure {
        code: code.into(),
        message: match code {
            "cancelled" => "送信許可・資格情報・ホスト終了により生成を停止しました",
            _ => "モデルとの接続または保存に失敗しました。再実行は自動で行いません",
        }
        .into(),
        details: json!({}),
    }
}
pub(crate) fn forget_credential(reference: String) {
    tokio::task::spawn_blocking(move || {
        let _ =
            keyring::Entry::new("ene-v2", &reference).and_then(|entry| entry.delete_credential());
    });
}

pub fn publish(boundary: &mut EventBoundary, app: &App, kind: &str, c: Option<&str>, data: Value) {
    boundary.cursor.seq += 1;
    let _ = app.events.send(Notification {
        epoch: boundary.cursor.epoch.clone(),
        seq: boundary.cursor.seq,
        kind: kind.into(),
        companion_id: c.map(str::to_string),
        data,
    });
}

pub(crate) async fn config(app: &App, controls: &mut Controls) -> Result<Configuration, Error> {
    if controls.config.is_none() {
        controls.config = Some(
            app.store
                .run(|conn| db::load(conn))
                .await
                .map_err(Error::from_store)?,
        );
    }
    Ok(controls.config.as_ref().unwrap().clone())
}

pub(crate) async fn commit(
    app: &App,
    controls: &mut Controls,
    mut c: Configuration,
) -> Result<(), Error> {
    db::refresh(&mut c);
    let saved = c.clone();
    let mut boundary = app.boundary.lock().await;
    app.store
        .run(move |conn| db::save(conn, &saved))
        .await
        .map_err(Error::from_store)?;
    publish(
        &mut boundary,
        app,
        "companion.updated",
        Some(&c.companion.id),
        json!(c.companion),
    );
    publish(
        &mut boundary,
        app,
        "settings.updated",
        None,
        json!(c.settings),
    );
    publish(&mut boundary, app, "grants.updated", None, json!(c.grants));
    controls.config = Some(c);
    Ok(())
}

pub(crate) fn update_connection(c: &mut Configuration, p: ProviderConnection) {
    if let Some(item) = c.settings.connections.iter_mut().find(|v| v["id"] == p.id) {
        *item = json!(p);
    }
}
pub(crate) fn active(controls: &Controls, id: &str) -> bool {
    controls.running.values().any(|r| r.connection == id)
}
pub(crate) fn cancel_connection(controls: &Controls, id: &str) {
    for run in controls.running.values().filter(|r| r.connection == id) {
        run.cancel.send_replace(true);
    }
}

pub(crate) fn cancel_purpose(controls: &Controls, id: &str, purpose: &str) {
    for run in controls
        .running
        .values()
        .filter(|r| r.connection == id && r.purpose == purpose)
    {
        run.cancel.send_replace(true);
    }
}

pub async fn initialize(app: &App) -> Result<(), Error> {
    let mut controls = app.controls.lock().await;
    let mut c = config(app, &mut controls).await?;
    let refs = c.credential_refs.clone();
    for v in c.settings.connections.clone() {
        let Ok(mut p) = serde_json::from_value::<ProviderConnection>(v) else {
            continue;
        };
        p.credential_state = "missing".into();
        if let Some(reference) = refs.get(&p.id) {
            let reference = reference.clone();
            let loaded = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || {
                    keyring::Entry::new("ene-v2", &reference).and_then(|entry| entry.get_password())
                }),
            )
            .await;
            if let Ok(Ok(Ok(secret))) = loaded {
                controls.secrets.insert(
                    p.id.clone(),
                    Secret {
                        value: secret,
                        nonce: Uuid::new_v4().to_string(),
                    },
                );
                p.credential_state = "stored".into();
            }
        }
        update_connection(&mut c, p);
    }
    commit(app, &mut controls, c).await
}

pub(crate) fn permitted(
    c: &Configuration,
    controls: &Controls,
    purpose: &str,
) -> Result<(ProviderConnection, String), Error> {
    let assignment = c
        .companion
        .model_assignments
        .companion
        .as_ref()
        .ok_or(Error {
            status: StatusCode::CONFLICT,
            code: "state_conflict",
            reason: Some("model_unset"),
        })?;
    let p = assignment["connection_id"]
        .as_str()
        .and_then(|id| db::connection(c, id))
        .ok_or_else(|| Error::new(StatusCode::CONFLICT, "state_conflict"))?;
    let secret = controls.secrets.get(&p.id).ok_or(Error {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "unavailable",
        reason: Some("credential_missing"),
    })?;
    let data = if purpose == "conversation" {
        vec!["conversation"]
    } else {
        vec!["test_input"]
    };
    if !db::allowed(c, &p, purpose, &data) {
        return Err(Error::new(StatusCode::FORBIDDEN, "permission_required"));
    }
    Ok((p, secret.value.clone()))
}

async fn update(
    app: &App,
    id: &str,
    status: MessageStatus,
    delta: Option<String>,
    error: Option<Failure>,
) -> Result<(), Error> {
    let mut boundary = app.boundary.lock().await;
    let saved_id = id.to_string();
    let saved_delta = delta.clone();
    let saved_status = status.clone();
    let message = app
        .store
        .run(move |conn| db::update_message(conn, &saved_id, saved_status, saved_delta, error))
        .await
        .map_err(Error::from_store)?;
    if let Some(delta) = delta {
        publish(
            &mut boundary,
            app,
            "message.delta",
            Some(&message.companion_id),
            json!({"message_id":id,"offset":message.text.chars().count()-delta.chars().count(),"delta":delta}),
        );
    } else {
        publish(
            &mut boundary,
            app,
            if status == MessageStatus::Streaming {
                "message.started"
            } else {
                "message.updated"
            },
            Some(&message.companion_id),
            json!(message),
        );
    }
    Ok(())
}

pub async fn generate(
    app: App,
    c: Configuration,
    secret: String,
    prompt: String,
    id: String,
    mut cancelled: watch::Receiver<bool>,
) {
    let work = async {
        update(&app, &id, MessageStatus::Streaming, None, None)
            .await
            .map_err(|_| "storage_error")?;
        let mut history = app
            .store
            .messages(c.companion.id.clone(), None, 100)
            .await
            .map_err(|_| "storage_error")?
            .items;
        let response = history
            .iter()
            .find(|m| m.id == id)
            .ok_or("storage_error")?
            .sequence;
        history.retain(|m| {
            m.sequence < response.saturating_sub(1)
                && m.status == MessageStatus::Completed
                && m.task_id.is_none()
        });
        let messages = history.into_iter().map(|m| {
            if m.role == Role::User {
                rig_core::message::Message::user(m.text)
            } else {
                rig_core::message::Message::assistant(m.text)
            }
        });
        let request = CompletionRequest::new(prompt)
            .preamble(c.companion.mind.clone())
            .messages(messages)
            .max_tokens(2048)
            .additional_params(json!({"provider":{"only":["openai"],"allow_fallbacks":false}}));
        let client = rig_core::providers::openrouter::new(secret);
        let mut stream = client
            .chat(MODEL)
            .stream(request)
            .map_err(|_| "upstream_error")?;
        let mut bytes = 0usize;
        while let Some(event) = stream.next().await {
            if let Item::Event(StreamEvent::Text { text, .. }) =
                event.map_err(|_| "upstream_error")?
            {
                bytes += text.len();
                if bytes > 128_000 {
                    return Err("output_limit");
                }
                update(&app, &id, MessageStatus::Streaming, Some(text), None)
                    .await
                    .map_err(|_| "storage_error")?;
            }
        }
        let finished = stream.finish().await.map_err(|_| "upstream_error")?;
        if !matches!(
            finished.finish_reason(),
            Some(rig_core::completion::FinishReason::Stop)
        ) {
            return Err("incomplete_output");
        }
        Ok::<_, &'static str>(())
    };
    let mut shutdown = app.shutdown.subscribe();
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        tokio::select! {biased;_ = cancelled.changed()=>Err("cancelled"),_ = shutdown.changed()=>Err("cancelled"),result=tokio::time::timeout(Duration::from_secs(180),work)=>result.unwrap_or(Err("upstream_timeout"))}
    };
    let (status, error) = match result {
        Ok(()) => (MessageStatus::Completed, None),
        Err("cancelled") => (MessageStatus::Cancelled, Some(failure("cancelled"))),
        Err(code) => (MessageStatus::Failed, Some(failure(code))),
    };
    let _ = update(&app, &id, status, None, error).await;
    app.controls.lock().await.running.remove(&id);
}
