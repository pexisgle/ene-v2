//! Enable/disable, startup restore and shutdown for host plugins.
use super::{Manager, conflict, failure, storage_failure, unsupported, verify};
use crate::{
    domain::{Error, ErrorKind},
    ports::{Result, plugin::Plugins},
};
use ene_plugin::Target;
use ene_plugin_runtime::instance::Instance;
use ene_rpc::EnablePluginRequest;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::atomic::Ordering};

impl Manager {
    pub(super) async fn change_enabled(
        &self,
        id: String,
        input: EnablePluginRequest,
    ) -> Result<()> {
        if input.enabled {
            let package = self
                .entries
                .read()
                .map_err(failure)?
                .get(&id)
                .ok_or_else(unsupported)?
                .package
                .clone();
            verify(package).await?;
        }
        let (package, generation, cancel) = {
            let _commit = self.commit.lock().await;
            let mut entries = self.entries.write().map_err(failure)?;
            let e = entries.get_mut(&id).ok_or_else(unsupported)?;
            if e.state == "unavailable" {
                return Err(unsupported());
            }
            if e.package.revision != input.revision {
                return Err(conflict());
            }
            e.cancellation.cancel();
            e.cancellation = self.stopped.child_token();
            e.generation = self.generations.fetch_add(1, Ordering::Relaxed);
            e.state = if input.enabled {
                "initializing"
            } else {
                "disabled"
            }
            .into();
            e.capabilities.clear();
            (e.package.clone(), e.generation, e.cancellation.clone())
        };
        // Cancelling old generations releases service waits before collecting workers.
        self.stop_sessions(Some(&id)).await?;
        let result = async {
            let mut capabilities = BTreeMap::new();
            if input.enabled {
                for entry in package
                    .manifest
                    .entrypoints
                    .iter()
                    .filter(|e| e.target == Target::Host)
                {
                    let verified = package.clone();
                    let worker = Instance::load(
                        verified,
                        entry.id.clone(),
                        Target::Host,
                        generation,
                        &cancel,
                    )
                    .await
                    .map_err(failure)?;
                    capabilities.insert(entry.id.clone(), worker.capabilities);
                }
            }
            Ok::<_, Error>(capabilities)
        }
        .await;
        let _commit = self.commit.lock().await;
        {
            let mut entries = self.entries.write().map_err(failure)?;
            let e = entries.get_mut(&id).ok_or_else(unsupported)?;
            if e.generation != generation {
                return Err(conflict());
            }
            match &result {
                Ok(c) => {
                    e.state = if input.enabled { "ready" } else { "disabled" }.into();
                    e.capabilities = c.clone();
                    e.error = None;
                }
                Err(_) => {
                    e.state = "failed".into();
                    e.error = Some("Wasmプラグインの読み込みまたは初期化に失敗しました".into());
                    e.cancellation.cancel();
                }
            }
        }
        if result.is_ok() {
            self.store
                .plugin_set("enabled".into(), id.clone(), json!({"id":id,"input":input}))
                .await
                .map_err(storage_failure)?;
        }
        result.map(|_| ())
    }
    pub(super) async fn restore(&self) -> Result<()> {
        // Guest memory cannot survive a host restart; finalize its saved tasks first.
        for binding in self.bindings().await? {
            self.harness_stop(&binding.id, "host_interrupted").await?;
        }
        let records = self
            .store
            .plugin_list("enabled".into())
            .await
            .map_err(storage_failure)?;
        // Saved identity includes the package ID; changed code never auto-enables.
        for record in records {
            if let (Some(id), Some(input)) = (
                record.get("id").and_then(Value::as_str),
                record.get("input"),
            ) && let Ok(input) = serde_json::from_value::<EnablePluginRequest>(input.clone())
                && let Err(error) = self.change_enabled(id.into(), input).await
                && error.kind == ErrorKind::StorageFailure
            {
                return Err(error);
            }
        }
        Ok(())
    }
}
