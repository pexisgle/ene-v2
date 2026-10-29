use super::{Boundary, HostApplication, Notification};
use crate::{
    domain::{rules, *},
    llm::*,
    ports::Secret,
};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;
pub struct Running {
    pub companion: String,
    pub connection: String,
    pub policy: Policy,
    pub nonce: Option<String>,
    pub operation: OperationKind,
    pub cancel: watch::Sender<bool>,
}
#[derive(Default)]
pub struct Controls {
    pub config: Option<Configuration>,
    pub secrets: BTreeMap<String, Secret>,
    pub running: BTreeMap<String, Running>,
    storage_fault: bool,
    pub tails: BTreeMap<String, watch::Receiver<Option<Result<(), Error>>>>,
}

fn failure(code: &'static str) -> Failure {
    Failure {
        code: code.into(),
        message: match code {
            "cancelled" => "停止操作・送信許可や資格情報の取消・ホスト終了により生成を停止しました",
            "input_limit" => "入力が送信上限を超えています。文章を短くしてください",
            _ => "モデルとの接続または保存に失敗しました。再実行は自動で行いません",
        }
        .into(),
        details: ene_rpc::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}
pub fn publish(
    boundary: &mut Boundary,
    app: &HostApplication,
    c: Option<&str>,
    event: ene_rpc::notification::Event,
) {
    if let Some(next) = boundary.cursor.seq.checked_add(1) {
        boundary.cursor.seq = next;
    } else {
        boundary.cursor.epoch = Uuid::new_v4().to_string();
        boundary.cursor.seq = 1;
    }
    let _ = app.0.events.send(Notification {
        cursor: boundary.cursor.clone().into(),
        companion_id: c.map(str::to_string),
        event: Some(event),
        ..Default::default()
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

pub(crate) use super::commit::commit;
impl Controls {
    /// A fault transition and its notification share the snapshot boundary.
    pub fn fail_storage(&mut self, app: &HostApplication, boundary: &mut Boundary) {
        if self.storage_fault {
            return;
        }
        self.storage_fault = true;
        publish(
            boundary,
            app,
            None,
            ene_rpc::notification::Event::HostStorageFailed(Box::new(
                ene_rpc::PersistenceFailure {
                    code: "storage_error".into(),
                    ..Default::default()
                },
            )),
        );
    }

    pub fn failed(&self) -> bool {
        self.storage_fault
    }
    pub fn admit(&self, app: &HostApplication) -> Result<(), Error> {
        if *app.0.shutdown.borrow() || app.0.generations.is_closed() {
            return Err(Error::unavailable());
        }
        if self.failed() {
            return Err(Error::new(ErrorKind::StorageFailure));
        }
        if self.running.len() >= MAX_EXECUTIONS {
            return Err(Error {
                kind: ErrorKind::StateConflict,
                reason: Some("busy"),
            });
        }
        Ok(())
    }
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
            && rules::allowed(
                c,
                &run.companion,
                &p,
                run.operation.spec().purpose.as_str(),
                run.operation.spec().data,
                &run.policy,
            )
        {
            continue;
        }
        run.cancel.send_replace(true);
    }
}
pub(crate) fn fingerprint(c: &ProviderConnection, providers: &dyn ProviderRegistry) -> Value {
    providers.scope_fingerprint(c).unwrap_or(Value::Null)
}
pub(crate) fn refresh(app: &HostApplication, controls: &Controls, c: &mut Configuration) {
    for mut companion in c.companions.values().cloned().collect::<Vec<_>>() {
        let policy = companion
            .model_assignments
            .companion
            .as_option()
            .and_then(|a| rules::connection(c, &a.connection_id))
            .and_then(|p| {
                app.0
                    .providers
                    .resolve(&p)
                    .ok()
                    .map(|binding| binding.policy().clone())
            });
        let credential_present = companion
            .model_assignments
            .companion
            .as_option()
            .is_some_and(|assignment| controls.secrets.contains_key(&assignment.connection_id));
        rules::refresh(
            &mut companion,
            c,
            policy.as_ref(),
            rules::ExecutionState {
                credential_present,
                healthy: !controls.failed(),
                stopping: *app.0.shutdown.borrow(),
                busy: false,
            },
        );
        c.companions.insert(companion.id.clone(), companion);
    }
}

mod authorization;
mod supervisor;
pub(super) use supervisor::execute;
mod initialization;
mod streaming;
pub(super) use authorization::{AuthorizedContext, authorize, check_run, invocation, register};
pub use streaming::generate;

pub(crate) fn invalidate_changed_grants(
    app: &HostApplication,
    c: &mut Configuration,
) -> Result<(), Error> {
    for grant in &mut c.grants {
        if grant.status == GrantStatus::Active
            && c.settings
                .connections
                .iter()
                .find(|p| p.id == grant.connection_id)
                .and_then(|p| app.0.providers.scope_fingerprint(p).ok())
                .is_some_and(|fingerprint| fingerprint != grant.connection_fingerprint)
        {
            grant.status = GrantStatus::Invalidated;
            grant.invalidation_reason = Some("provider_changed".into());
            grant.revision = next_revision(grant.revision)?;
        }
    }
    Ok(())
}
