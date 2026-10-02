use super::{Boundary, HostApplication, Notification};
use crate::{
    domain::{rules, *},
    ports::{Secret, llm::*},
};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;
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
pub fn publish(
    boundary: &mut Boundary,
    app: &HostApplication,
    kind: &str,
    c: Option<&str>,
    data: Value,
) {
    boundary.cursor.seq += 1;
    let _ = app.0.events.send(Notification {
        epoch: boundary.cursor.epoch.clone(),
        seq: boundary.cursor.seq,
        kind: kind.into(),
        companion_id: c.map(str::to_string),
        data,
    });
}

pub(crate) async fn config(
    app: &HostApplication,
    controls: &mut Controls,
) -> Result<Configuration, Error> {
    if controls.config.is_none() {
        controls.config = Some(app.0.repository.configuration().await?);
    }
    Ok(controls.config.as_ref().unwrap().clone())
}

pub(crate) async fn commit(
    app: &HostApplication,
    controls: &mut Controls,
    mut c: Configuration,
) -> Result<(), Error> {
    refresh(app, &mut c);
    let saved = c.clone();
    let mut boundary = app.0.boundary.lock().await;
    app.0.repository.save_configuration(saved, None).await?;
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
    if let Some(item) = c.settings.connections.iter_mut().find(|v| v.id == p.id) {
        *item = p;
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

pub async fn initialize(app: &HostApplication) -> Result<(), Error> {
    let mut controls = app.0.controls.lock().await;
    let mut c = config(app, &mut controls).await?;
    for mut p in c.settings.connections.clone() {
        p.credential_state = "missing".into();
        if let Some(reference) = c.credential_refs.get(&p.id)
            && let Ok(value) = app.0.credentials.load(reference.clone()).await
        {
            controls.secrets.insert(
                p.id.clone(),
                Secret {
                    value,
                    nonce: Uuid::new_v4().to_string(),
                },
            );
            p.credential_state = "stored".into();
        }
        update_connection(&mut c, p);
    }
    commit(app, &mut controls, c).await
}

pub(crate) fn permitted(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    p: &ProviderConnection,
    purpose: &str,
) -> Result<Option<Secret>, Error> {
    let data = if purpose == "conversation" {
        vec!["conversation"]
    } else {
        vec!["test_input"]
    };
    permitted_data(app, c, controls, p, purpose, &data)
}
pub(crate) fn permitted_data(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    p: &ProviderConnection,
    purpose: &str,
    data: &[&str],
) -> Result<Option<Secret>, Error> {
    let policy = app.0.providers.policy(p)?;
    let secret = controls.secrets.get(&p.id).cloned();
    if policy.requires_credentials && secret.is_none() {
        return Err(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("credential_missing"),
        });
    }
    if !rules::allowed(c, p, purpose, data, &policy) {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    Ok(secret)
}

async fn update(
    app: &HostApplication,
    id: &str,
    status: MessageStatus,
    delta: Option<String>,
    error: Option<Failure>,
) -> Result<(), Error> {
    let controls = app.0.controls.lock().await;
    if status == MessageStatus::Streaming
        && controls.running.get(id).is_none_or(|r| *r.cancel.borrow())
    {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let status = if status == MessageStatus::Completed
        && controls.running.get(id).is_none_or(|r| *r.cancel.borrow())
    {
        MessageStatus::Cancelled
    } else {
        status
    };
    let mut boundary = app.0.boundary.lock().await;
    let saved_id = id.to_string();
    let saved_delta = delta.clone();
    let saved_status = status.clone();
    let message = app
        .0
        .repository
        .update_message(saved_id, saved_status, saved_delta, error)
        .await?;
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

/// Rechecks the host-selected context at every external send. Plugins cannot supply purpose.
struct Authority {
    app: HostApplication,
    id: String,
    connection: ProviderConnection,
    purpose: &'static str,
    nonce: Option<String>,
    fingerprint: Value,
    data: Vec<&'static str>,
}
impl Authorization for Authority {
    fn check(&self) -> BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            let controls = self.app.0.controls.lock().await;
            let c = controls.config.as_ref().ok_or_else(Error::unavailable)?;
            let run = controls
                .running
                .get(&self.id)
                .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
            let current = rules::connection(c, &self.connection.id)
                .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
            if *run.cancel.borrow()
                || *self.app.0.shutdown.borrow()
                || fingerprint(&current, self.app.0.providers.as_ref()) != self.fingerprint
                || controls.secrets.get(&current.id).map(|s| s.nonce.clone()) != self.nonce
            {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            permitted_data(&self.app, c, &controls, &current, self.purpose, &self.data)?;
            Ok(())
        })
    }
}
pub(super) fn invocation(
    app: &HostApplication,
    p: ProviderConnection,
    secret: Option<Secret>,
    id: String,
    purpose: &'static str,
    request: Generation,
    cancel: watch::Receiver<bool>,
) -> Invocation {
    let authorization = std::sync::Arc::new(Authority {
        app: app.clone(),
        id: id.clone(),
        connection: p.clone(),
        purpose,
        nonce: secret.as_ref().map(|s| s.nonce.clone()),
        fingerprint: fingerprint(&p, app.0.providers.as_ref()),
        data: if request.max_tokens == 0 {
            vec!["metadata"]
        } else if purpose == "conversation" {
            vec!["conversation"]
        } else {
            vec!["test_input"]
        },
    });
    Invocation {
        id,
        connection: p,
        secret,
        request,
        authorization,
        cancel,
    }
}
pub async fn generate(
    app: HostApplication,
    c: Configuration,
    p: ProviderConnection,
    secret: Option<Secret>,
    prompt: String,
    id: String,
    mut cancelled: watch::Receiver<bool>,
) {
    let provider_cancel = cancelled.clone();
    let work = async {
        update(&app, &id, MessageStatus::Streaming, None, None)
            .await
            .map_err(|_| "storage_error")?;
        let mut history = app
            .0
            .repository
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
        let mut messages = history
            .into_iter()
            .map(|m| ChatMessage {
                role: if m.role == Role::User {
                    "user"
                } else {
                    "assistant"
                }
                .into(),
                text: m.text,
            })
            .collect::<Vec<_>>();
        messages.push(ChatMessage {
            role: "user".into(),
            text: prompt,
        });
        let model = c
            .companion
            .model_assignments
            .companion
            .as_ref()
            .ok_or("model_unset")?
            .model_id
            .clone();
        let request = Generation {
            endpoint: p.endpoint.clone(),
            options: p.options.clone(),
            model,
            preamble: c.companion.mind.clone(),
            messages,
            max_tokens: 2048,
        };
        let input = invocation(
            &app,
            p,
            secret,
            id.clone(),
            "conversation",
            request,
            provider_cancel,
        );
        let mut stream = app
            .0
            .providers
            .generate(input)
            .await
            .map_err(|_| "upstream_error")?;
        let mut bytes = 0;
        let mut finished = false;
        while let Some(event) = stream.next().await {
            match event.map_err(|_| "upstream_error")? {
                Event::Delta(text) => {
                    bytes += text.len();
                    if bytes > 128_000 {
                        return Err("output_limit");
                    }
                    update(&app, &id, MessageStatus::Streaming, Some(text), None)
                        .await
                        .map_err(|_| "storage_error")?;
                }
                Event::Finished(reason) => {
                    if reason != "stop" {
                        return Err("incomplete_output");
                    }
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            return Err("incomplete_output");
        }
        Ok::<_, &'static str>(())
    };
    let mut shutdown = app.0.shutdown.subscribe();
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        tokio::select! {biased;_=cancelled.changed()=>Err("cancelled"),_=shutdown.changed()=>Err("cancelled"),r=tokio::time::timeout(Duration::from_secs(180),work)=>r.unwrap_or(Err("upstream_timeout"))}
    };
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        result
    };
    let (status, error) = match result {
        Ok(()) => (MessageStatus::Completed, None),
        Err("cancelled") => (MessageStatus::Cancelled, Some(failure("cancelled"))),
        Err(code) => (MessageStatus::Failed, Some(failure(code))),
    };
    let _ = update(&app, &id, status.clone(), None, error).await;
    let mut controls = app.0.controls.lock().await;
    controls.running.remove(&id);
    if status == MessageStatus::Failed
        && let Some(c) = controls.config.clone()
    {
        let _ = commit(&app, &mut controls, c).await;
    }
}
impl HostApplication {
    pub async fn initialize(&self) -> Result<(), Error> {
        initialize(self).await
    }
}

pub(crate) fn fingerprint(c: &ProviderConnection, providers: &dyn ProviderRegistry) -> Value {
    providers
        .policy(c)
        .map(|p| p.fingerprint)
        .unwrap_or(Value::Null)
}
pub(crate) fn allowed(
    app: &HostApplication,
    c: &Configuration,
    p: &ProviderConnection,
    purpose: &str,
    data: &[&str],
) -> bool {
    app.0
        .providers
        .policy(p)
        .is_ok_and(|policy| rules::allowed(c, p, purpose, data, &policy))
}
pub(crate) fn refresh(app: &HostApplication, c: &mut Configuration) {
    let policy = c
        .companion
        .model_assignments
        .companion
        .as_ref()
        .and_then(|a| rules::connection(c, &a.connection_id))
        .and_then(|p| app.0.providers.policy(&p).ok());
    rules::refresh(c, policy.as_ref());
}
