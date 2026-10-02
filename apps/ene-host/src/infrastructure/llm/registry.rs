//! Manifest discovery never starts a process. Enabling probes the isolated runtime.
use super::{
    openai,
    plugin::{
        manifest::{self, Bundle},
        process,
    },
};
use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
};
use futures_util::{future::BoxFuture, stream::BoxStream};
use std::{collections::BTreeMap, path::PathBuf, sync::RwLock};
#[derive(Clone)]
struct Entry {
    bundle: Bundle,
    state: String,
    capabilities: Capabilities,
    error: Option<String>,
    operation: u64,
    initialization: Option<tokio::sync::watch::Sender<bool>>,
}
pub struct Registry {
    root: PathBuf,
    entries: std::sync::Arc<RwLock<BTreeMap<String, Entry>>>,
    supervisor: process::Supervisor,
    discovery_error: RwLock<Option<String>>,
}
impl Registry {
    pub fn discover() -> Self {
        let root = std::env::var_os("ENE_PROVIDER_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                xdg::BaseDirectories::new()
                    .get_data_home()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("ene/providers")
            });
        let r = Self {
            root,
            entries: std::sync::Arc::new(RwLock::new(BTreeMap::new())),
            supervisor: process::Supervisor::default(),
            discovery_error: RwLock::new(None),
        };
        if r.scan().is_err() {
            *r.discovery_error.write().expect("registry lock") = Some("invalid_manifest".into());
        }
        r
    }
    fn scan(&self) -> Result<()> {
        let mut next = BTreeMap::new();
        let old = self
            .entries
            .read()
            .map_err(|_| Error::unavailable())?
            .clone();
        let paths = match std::fs::read_dir(&self.root) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *self.entries.write().map_err(|_| Error::unavailable())? = next;
                return Ok(());
            }
            Err(_) => return Err(Error::invalid()),
        };
        for path in paths {
            let path = path.map_err(|_| Error::invalid())?.path();
            let bundle = manifest::load(&path)?;
            if next.contains_key(&bundle.manifest.id) {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            let entry = old
                .get(&bundle.manifest.id)
                .filter(|e| e.bundle.revision == bundle.revision)
                .cloned()
                .unwrap_or(Entry {
                    bundle: bundle.clone(),
                    state: "disabled".into(),
                    capabilities: Capabilities::default(),
                    error: None,
                    operation: 0,
                    initialization: None,
                });
            next.insert(bundle.manifest.id.clone(), entry);
        }
        *self.entries.write().map_err(|_| Error::unavailable())? = next;
        Ok(())
    }
    fn provider(&self, id: &str) -> Result<std::sync::Arc<dyn LlmProvider>> {
        if id == openai::ID {
            Ok(std::sync::Arc::new(openai::Provider))
        } else {
            Ok(std::sync::Arc::new(super::plugin::Provider {
                bundle: self.ready(id)?,
                supervisor: self.supervisor.clone(),
            }))
        }
    }
    fn ready(&self, id: &str) -> Result<Bundle> {
        let entries = self.entries.read().map_err(|_| Error::unavailable())?;
        let e = entries
            .get(id)
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
        if e.state != "ready" {
            return Err(Error::unavailable());
        }
        if manifest::load(&e.bundle.root)?.revision != e.bundle.revision {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        Ok(e.bundle.clone())
    }
}
impl ProviderRegistry for Registry {
    fn descriptors(&self) -> Vec<Descriptor> {
        let mut v = vec![openai::descriptor()];
        if let Ok(entries) = self.entries.read() {
            v.extend(entries.values().map(|e| {
                if manifest::load(&e.bundle.root)
                    .is_ok_and(|bundle| bundle.revision == e.bundle.revision)
                {
                    e.bundle
                        .descriptor(&e.state, e.capabilities.clone(), e.error.clone())
                } else {
                    e.bundle.descriptor(
                        "changed",
                        Capabilities::default(),
                        Some("実装が変更されています。再走査して有効化してください".into()),
                    )
                }
            }));
        }
        if self
            .discovery_error
            .read()
            .ok()
            .is_some_and(|e| e.is_some())
        {
            v[0].error =
                Some("プラグインのmanifestを読み込めません。修正後に再走査してください".into());
        }
        v
    }
    fn policy(&self, c: &Connection) -> Result<Policy> {
        if c.adapter == openai::ID {
            openai::policy(c)
        } else {
            self.ready(&c.adapter)?.policy(c)
        }
    }
    fn generate(
        &self,
        input: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
        Box::pin(async move {
            let id = input.connection.adapter.clone();
            let provider = self.provider(&id)?;
            provider.policy(&input.connection)?;
            let stream = provider.generate(input).await?;
            let entries = self.entries.clone();
            use futures_util::StreamExt;
            Ok(Box::pin(stream.map(move |event| {
                if event
                    .as_ref()
                    .is_err_and(|e| e.kind == ErrorKind::ExternalFailure)
                    && let Ok(mut entries) = entries.write()
                    && let Some(e) = entries.get_mut(&id)
                {
                    e.state = "failed".into();
                    e.capabilities = Capabilities::default();
                    e.error = Some("プラグインの通信に失敗しました。再度有効化してください".into());
                }
                event
            })) as _)
        })
    }
    fn models(&self, input: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async move {
            let provider = self.provider(&input.connection.adapter)?;
            provider.policy(&input.connection)?;
            provider.models(input).await
        })
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.supervisor.shutdown())
    }
    fn reload(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let result = self.scan();
            *self
                .discovery_error
                .write()
                .map_err(|_| Error::unavailable())? =
                result.as_ref().err().map(|_| "invalid_manifest".into());
            result
        })
    }
    fn enable(&self, id: String, enabled: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if id == openai::ID {
                return if enabled {
                    Ok(())
                } else {
                    Err(Error::new(ErrorKind::UnsupportedCapability))
                };
            }
            let (bundle, ticket, cancelled) = {
                let mut entries = self.entries.write().map_err(|_| Error::unavailable())?;
                let e = entries
                    .get_mut(&id)
                    .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
                if let Some(cancel) = e.initialization.take() {
                    cancel.send_replace(true);
                }
                e.operation += 1;
                e.state = if enabled { "initializing" } else { "disabled" }.into();
                e.capabilities = Capabilities::default();
                let (cancel, receiver) = tokio::sync::watch::channel(false);
                if enabled {
                    e.initialization = Some(cancel);
                }
                (e.bundle.clone(), e.operation, receiver)
            };
            let capabilities = if enabled {
                process::probe_with_cancel(&bundle, cancelled).await
            } else {
                Ok(Capabilities::default())
            };
            let mut entries = self.entries.write().map_err(|_| Error::unavailable())?;
            let e = entries
                .get_mut(&id)
                .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
            if e.bundle.revision != bundle.revision || e.operation != ticket {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            e.initialization = None;
            match capabilities {
                Ok(c) => {
                    e.state = if enabled { "ready" } else { "disabled" }.into();
                    e.capabilities = c;
                    e.error = None;
                    Ok(())
                }
                Err(error) => {
                    e.state = "failed".into();
                    e.capabilities = Capabilities::default();
                    e.error = Some("隔離環境での起動または通信初期化に失敗しました".into());
                    Err(error)
                }
            }
        })
    }
}
