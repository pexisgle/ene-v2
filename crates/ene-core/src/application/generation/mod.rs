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
    pub companion: String,
    pub connection: String,
    pub data: Vec<&'static str>,
    pub policy: Policy,
    pub terminal_error: Option<Error>,
    pub purpose: &'static str,
    pub cancel: watch::Sender<bool>,
}
#[derive(Default)]
pub struct Controls {
    pub config: Option<Configuration>,
    pub secrets: BTreeMap<String, Secret>,
    pub running: BTreeMap<String, Running>,
    pub tails: BTreeMap<String, watch::Receiver<Option<Result<(), Error>>>>,
}

fn failure(code: &'static str) -> Failure {
    Failure {
        code: code.into(),
        message: match code {
            "cancelled" => "送信許可・資格情報・ホスト終了により生成を停止しました",
            "input_limit" => "入力が送信上限を超えています。文章を短くしてください",
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
    for companion in c.companions.values() {
        publish(
            &mut boundary,
            app,
            "companion.updated",
            Some(&companion.id),
            json!(companion),
        );
    }
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

pub(crate) fn cancel_unpermitted(controls: &Controls, c: &Configuration) {
    for run in controls.running.values() {
        if let Some(p) = rules::connection(c, &run.connection)
            && rules::allowed(c, &run.companion, &p, run.purpose, &run.data, &run.policy)
        {
            continue;
        }
        run.cancel.send_replace(true);
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
    companion_id: &str,
    p: &ProviderConnection,
    purpose: &str,
    data: &[&str],
) -> bool {
    app.0
        .providers
        .policy(p)
        .is_ok_and(|policy| rules::allowed(c, companion_id, p, purpose, data, &policy))
}
pub(crate) fn refresh(app: &HostApplication, c: &mut Configuration) {
    for mut companion in c.companions.values().cloned().collect::<Vec<_>>() {
        let policy = companion
            .model_assignments
            .companion
            .as_ref()
            .and_then(|a| rules::connection(c, &a.connection_id))
            .and_then(|p| app.0.providers.policy(&p).ok());
        rules::refresh(&mut companion, c, policy.as_ref());
        c.companions.insert(companion.id.clone(), companion);
    }
}

mod authorization;
mod initialization;
mod streaming;
pub(super) use authorization::{Operation, invocation};
pub(crate) use authorization::{permitted, permitted_data};
pub use streaming::generate;
