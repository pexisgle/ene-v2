//! Enable/disable, startup restore and shutdown for host plugins.
use super::{Manager, conflict, failure, storage_failure, unsupported, verify};
use crate::{
    domain::{Error, ErrorKind},
    ports::Result,
};
use ene_plugin::Target;
use ene_plugin_runtime::instance::Instance;
use ene_rpc::EnablePluginRequest;
use std::{collections::BTreeMap, sync::atomic::Ordering};

impl Manager {
    pub async fn enable(&self, id: String, mut input: EnablePluginRequest) -> Result<()> {
        #[cfg(test)]
        self.enable_entries.fetch_add(1, Ordering::SeqCst);
        // The saved record names its own plugin, so restore needs no second key.
        input.id = id.clone();
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
                    let worker = Instance::load(verified, entry.id.clone(), generation, &cancel)
                        .await
                        .map_err(failure)?;
                    if let Some(declared) = worker.capabilities {
                        capabilities.insert(entry.id.clone(), declared);
                    }
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
                .plugin_set("enabled".into(), id.clone(), input)
                .await
                .map_err(storage_failure)?;
        }
        result.map(|_| ())
    }
    pub(crate) async fn restore(&self) -> Result<()> {
        // Guest memory cannot survive a host restart; finalize its saved tasks first.
        for binding in self.bindings().await? {
            self.harness_stop(&binding.id, "host_interrupted").await?;
        }
        let records: Vec<EnablePluginRequest> = self
            .store
            .plugin_list("enabled".into())
            .await
            .map_err(storage_failure)?;
        // Saved identity includes the package ID; changed code never auto-enables.
        for input in records {
            let id = input.id.clone();
            if let Err(error) = self.enable(id, input).await
                && error.kind == ErrorKind::StorageFailure
            {
                return Err(error);
            }
        }
        Ok(())
    }
}
