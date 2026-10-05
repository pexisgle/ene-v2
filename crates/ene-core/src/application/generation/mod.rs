use super::{
    HostApplication,
    changes::{Change, publish},
};
use crate::{
    domain::{rules, *},
    ports::{Secret, llm::*},
};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;
pub(super) struct Running {
    pub companion: String,
    pub connection: String,
    pub policy: Policy,
    pub terminal_error: Option<Error>,
    pub operation: Operation,
    pub cancel: watch::Sender<bool>,
}
#[derive(Default)]
pub(super) struct Controls {
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
    c: Configuration,
) -> Result<(), Error> {
    super::configuration::commit(app, controls, c, None).await
}

pub(crate) fn update_connection(c: &mut Configuration, p: ProviderConnection) {
    if let Some(item) = c.settings.connections.iter_mut().find(|v| v.id == p.id) {
        *item = p;
    }
}
pub(crate) fn active(controls: &Controls, id: &str) -> bool {
    controls.running.values().any(|r| r.connection == id)
}
pub(super) fn is_cancelled(app: &HostApplication, controls: &Controls, id: &str) -> bool {
    *app.0.shutdown.borrow()
        || controls
            .running
            .get(id)
            .is_none_or(|run| *run.cancel.borrow())
}
pub(crate) fn cancel_connection(controls: &Controls, id: &str) {
    for run in controls.running.values().filter(|r| r.connection == id) {
        run.cancel.send_replace(true);
    }
}

pub(crate) fn cancel_unpermitted(app: &HostApplication, controls: &Controls, c: &Configuration) {
    for run in controls.running.values() {
        if let Some(p) = rules::connection(c, &run.connection)
            && let Ok(policy) = app.0.providers.policy(&p)
            && policy.fingerprint == run.policy.fingerprint
            && rules::allowed(
                c,
                &run.companion,
                &p,
                run.operation.purpose(),
                run.operation.data(),
                &policy,
            )
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

/// Release the slot after a successful terminal write; retain and publish storage faults.
pub(super) async fn settle(
    app: &HostApplication,
    controls: &mut Controls,
    id: &str,
    persisted: Result<(), Error>,
) -> Result<(), Error> {
    match persisted {
        Ok(()) => {
            if let Some(run) = controls.running.remove(id) {
                run.cancel.send_replace(true);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(run) = controls.running.get_mut(id) {
                run.terminal_error = Some(error.clone());
                run.cancel.send_replace(true);
                let change = if run.operation == Operation::Conversation {
                    Change::PersistenceFailed {
                        companion: run.companion.clone(),
                        message: id.into(),
                    }
                } else {
                    Change::RequestPersistenceFailed {
                        companion: run.companion.clone(),
                        request: id.into(),
                    }
                };
                let mut boundary = app.0.boundary.lock().await;
                publish(&mut boundary, app, change);
            }
            Err(error)
        }
    }
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
mod execution;
pub(super) use execution::RunFailure;
mod initialization;
mod streaming;
pub(super) use authorization::{Operation, authorize, invocation, prepare};
pub use streaming::generate;
