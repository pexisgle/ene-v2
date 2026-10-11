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
    state: State,
    operation: u64,
}
#[derive(Clone)]
enum State {
    Disabled,
    Initializing(tokio::sync::watch::Sender<bool>),
    Ready(Capabilities),
    Failed(String),
}
impl State {
    fn descriptor(&self, bundle: &Bundle) -> Descriptor {
        match self {
            Self::Disabled => bundle.descriptor("disabled", Capabilities::default(), None),
            Self::Initializing(_) => {
                bundle.descriptor("initializing", Capabilities::default(), None)
            }
            Self::Ready(c) => bundle.descriptor("ready", c.clone(), None),
            Self::Failed(error) => {
                bundle.descriptor("failed", Capabilities::default(), Some(error.clone()))
            }
        }
    }
}
pub struct Registry {
    root: PathBuf,
    plugins: Option<std::sync::Arc<crate::infrastructure::plugins::Manager>>,
    entries: std::sync::Arc<RwLock<BTreeMap<String, Entry>>>,
    supervisor: process::Supervisor,
    discovery_errors: RwLock<Vec<Descriptor>>,
}
fn observe_failure(
    entries: &RwLock<BTreeMap<String, Entry>>,
    id: &str,
    ticket: &Option<(String, u64)>,
    error: &Error,
) {
    if !matches!(
        error.kind,
        ErrorKind::ExternalFailure | ErrorKind::StateConflict
    ) {
        return;
    }
    if let Ok(mut entries) = entries.write()
        && let Some(entry) = entries.get_mut(id)
        && ticket.as_ref() == Some(&(entry.bundle.revision.clone(), entry.operation))
    {
        entry.state = State::Failed(
            "プラグインの起動または通信に失敗しました。再度有効化してください".into(),
        );
    }
}
impl Registry {
    #[cfg(test)]
    pub fn discover() -> Self {
        Self::with_plugins(None)
    }
    pub fn with_plugins(
        plugins: Option<std::sync::Arc<crate::infrastructure::plugins::Manager>>,
    ) -> Self {
        let root = std::env::var_os("ENE_PROVIDER_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(ene_platform::paths::providers);
        Self::at_with_plugins(root, plugins)
    }
    #[cfg(test)]
    fn at(root: PathBuf) -> Self {
        Self::at_with_plugins(root, None)
    }
    fn at_with_plugins(
        root: PathBuf,
        plugins: Option<std::sync::Arc<crate::infrastructure::plugins::Manager>>,
    ) -> Self {
        let r = Self {
            root,
            plugins,
            entries: std::sync::Arc::new(RwLock::new(BTreeMap::new())),
            supervisor: process::Supervisor::default(),
            discovery_errors: RwLock::new(Vec::new()),
        };
        let _ = r.reload_scan();
        r
    }
    fn scan(&self) -> Result<()> {
        let mut next = BTreeMap::new();
        let mut errors = Vec::new();
        let mut duplicates = std::collections::BTreeSet::new();
        let old = self
            .entries
            .read()
            .map_err(|_| Error::unavailable())?
            .clone();
        let paths = match std::fs::read_dir(&self.root) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *self.entries.write().map_err(|_| Error::unavailable())? = next;
                self.discovery_errors
                    .write()
                    .map_err(|_| Error::unavailable())?
                    .clear();
                return Ok(());
            }
            Err(_) => return Err(Error::invalid()),
        };
        let mut paths = paths
            .map(|p| p.map(|p| p.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|_| Error::invalid())?;
        paths.sort();
        for path in paths {
            let bundle = match manifest::load(&path) {
                Ok(bundle) => bundle,
                Err(_) => {
                    errors.push(diagnostic(&path, "manifestまたはパッケージが不正です"));
                    continue;
                }
            };
            let id = &bundle.manifest.id;
            if next.contains_key(id)
                || duplicates.contains(id)
                || self
                    .plugins
                    .as_ref()
                    .is_some_and(|p| p.provider_entry(id).is_some())
            {
                next.remove(id);
                if duplicates.insert(id.clone()) {
                    errors.push(bundle.descriptor(
                        "failed",
                        Capabilities::default(),
                        Some("同じIDのパッケージが複数あります。重複を解消してください".into()),
                    ));
                }
                continue;
            }
            let entry = old
                .get(&bundle.manifest.id)
                .filter(|e| e.bundle.revision == bundle.revision)
                .cloned()
                .unwrap_or(Entry {
                    bundle: bundle.clone(),
                    state: State::Disabled,
                    operation: 0,
                });
            next.insert(bundle.manifest.id.clone(), entry);
        }
        *self.entries.write().map_err(|_| Error::unavailable())? = next;
        *self
            .discovery_errors
            .write()
            .map_err(|_| Error::unavailable())? = errors;
        Ok(())
    }
    fn reload_scan(&self) -> Result<()> {
        let result = self.scan();
        if result.is_err() {
            *self
                .discovery_errors
                .write()
                .map_err(|_| Error::unavailable())? = vec![diagnostic(
                &self.root,
                "プロバイダーディレクトリを走査できません",
            )];
        }
        result
    }
    // Authorization uses the last scanned bundle. The execution sandbox revalidates
    // its revision before starting code; policy queries must never perform file I/O.
    fn bundle(&self, id: &str) -> Result<Bundle> {
        self.entries
            .read()
            .map_err(|_| Error::unavailable())?
            .get(id)
            .map(|entry| entry.bundle.clone())
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))
    }
    #[cfg(test)]
    fn ready(&self, id: &str) -> Result<Bundle> {
        let entries = self.entries.read().map_err(|_| Error::unavailable())?;
        let e = entries
            .get(id)
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
        if !matches!(e.state, State::Ready(_)) {
            return Err(Error::unavailable());
        }
        Ok(e.bundle.clone())
    }
}
struct ObservedProvider {
    provider: std::sync::Arc<dyn LlmProvider>,
    entries: std::sync::Arc<RwLock<BTreeMap<String, Entry>>>,
    id: String,
    ticket: Option<(String, u64)>,
}
impl LlmProvider for ObservedProvider {
    fn policy(&self, connection: &Connection) -> Result<Policy> {
        self.provider.policy(connection)
    }
    fn generate(
        &self,
        input: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
        Box::pin(async move {
            let stream = match self.provider.generate(input).await {
                Ok(stream) => stream,
                Err(error) => {
                    observe_failure(&self.entries, &self.id, &self.ticket, &error);
                    return Err(error);
                }
            };
            let entries = self.entries.clone();
            let id = self.id.clone();
            let ticket = self.ticket.clone();
            use futures_util::StreamExt;
            Ok(Box::pin(stream.map(move |event| {
                if let Err(error) = &event {
                    observe_failure(&entries, &id, &ticket, error);
                }
                event
            })) as _)
        })
    }
    fn models(&self, input: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async move {
            let result = self.provider.models(input).await;
            if let Err(error) = &result {
                observe_failure(&self.entries, &self.id, &self.ticket, error);
            }
            result
        })
    }
}

impl ProviderRegistry for Registry {
    fn descriptors(&self) -> Vec<Descriptor> {
        let mut v = vec![openai::descriptor()];
        if let Ok(entries) = self.entries.read() {
            v.extend(entries.values().map(|e| e.state.descriptor(&e.bundle)));
        }
        if let Ok(errors) = self.discovery_errors.read() {
            v.extend(errors.iter().cloned());
        }
        if let Some(plugins) = &self.plugins {
            v.extend(plugins.descriptors());
        }
        v
    }
    fn policy(&self, c: &Connection) -> Result<Policy> {
        if c.adapter == openai::ID {
            return ProviderBinding::resolve(std::sync::Arc::new(openai::Provider), c)
                .map(|binding| binding.policy().clone());
        }
        if let Some(plugins) = &self.plugins
            && plugins.provider_entry(&c.adapter).is_some()
        {
            return plugins.provider_policy(c).map(|p| p.bind_connection(c));
        }
        Ok(self.bundle(&c.adapter)?.policy(c)?.bind_connection(c))
    }
    fn scope_fingerprint(&self, c: &Connection) -> Result<serde_json::Value> {
        if c.adapter == openai::ID {
            return self.policy(c).map(|p| p.fingerprint);
        }
        if let Some(plugins) = &self.plugins
            && plugins.provider_entry(&c.adapter).is_some()
        {
            return plugins
                .provider_identity(c)
                .map(|p| p.bind_connection(c).fingerprint);
        }
        Ok(self
            .bundle(&c.adapter)?
            .identity_policy(c)
            .bind_connection(c)
            .fingerprint)
    }
    fn resolve(&self, c: &Connection) -> Result<ProviderBinding> {
        if c.adapter == openai::ID {
            return ProviderBinding::resolve(std::sync::Arc::new(openai::Provider), c);
        }
        if let Some(plugins) = &self.plugins
            && plugins.provider_entry(&c.adapter).is_some()
        {
            return ProviderBinding::resolve(
                plugins
                    .provider(&c.adapter)
                    .ok_or_else(Error::unavailable)?,
                c,
            );
        }
        let entries = self.entries.read().map_err(|_| Error::unavailable())?;
        let entry = entries
            .get(&c.adapter)
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
        if !matches!(entry.state, State::Ready(_)) {
            return Err(Error::unavailable());
        }
        let provider = ObservedProvider {
            provider: std::sync::Arc::new(super::plugin::Provider {
                bundle: entry.bundle.clone(),
                supervisor: self.supervisor.clone(),
            }),
            entries: self.entries.clone(),
            id: c.adapter.clone(),
            ticket: Some((entry.bundle.revision.clone(), entry.operation)),
        };
        ProviderBinding::resolve(std::sync::Arc::new(provider), c)
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.supervisor.shutdown())
    }
    fn reload(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(plugins) = &self.plugins {
                use ene_core::ports::plugin::Plugins;
                plugins.reload().await?;
            }
            self.reload_scan()
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
            if let Some(plugins) = &self.plugins
                && plugins.provider_entry(&id).is_some()
            {
                return plugins.enable_provider(&id, enabled).await;
            }
            let (bundle, ticket, cancelled) = {
                let mut entries = self.entries.write().map_err(|_| Error::unavailable())?;
                let e = entries
                    .get_mut(&id)
                    .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
                if let State::Initializing(cancel) = &e.state {
                    cancel.send_replace(true);
                }
                e.operation += 1;
                let (cancel, receiver) = tokio::sync::watch::channel(false);
                e.state = if enabled {
                    State::Initializing(cancel)
                } else {
                    State::Disabled
                };
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
            match capabilities {
                Ok(c) => {
                    e.state = if enabled {
                        State::Ready(c)
                    } else {
                        State::Disabled
                    };
                    Ok(())
                }
                Err(error) => {
                    e.state =
                        State::Failed("隔離環境での起動または通信初期化に失敗しました".into());
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_queries_use_scanned_bundle_until_reload() {
        let root = crate::test_support::package_directory();
        let path = root.path().join("fixture");
        crate::test_support::create_directory(&path).unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        crate::test_support::write(path.join("manifest.json"), manifest.to_string()).unwrap();
        let registry = Registry {
            plugins: None,
            root: root.path().into(),
            entries: Default::default(),
            supervisor: process::Supervisor::default(),
            discovery_errors: Default::default(),
        };
        registry.scan().unwrap();
        let connection = Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: "fixture".into(),
            endpoint: "https://example.com/v1".into(),
            options: serde_json::json!({}),
            credential_state: "missing".into(),
            capabilities: None,
            last_check: None,
        };
        let policy = registry.policy(&connection).unwrap();
        let scope = registry.scope_fingerprint(&connection).unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        assert_eq!(
            registry.policy(&connection).unwrap().fingerprint,
            policy.fingerprint
        );
        assert_eq!(registry.scope_fingerprint(&connection).unwrap(), scope);
        registry.scan().unwrap();
        assert!(registry.scope_fingerprint(&connection).is_err());
    }
    #[test]
    fn request_failure_keeps_plugin_ready_and_old_process_cannot_poison_reenabled_plugin() {
        let dir = crate::test_support::package_directory();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        crate::test_support::write(dir.path().join("manifest.json"), manifest.to_string()).unwrap();
        let bundle = manifest::load(dir.path()).unwrap();
        let revision = bundle.revision.clone();
        let ticket = Some((revision.clone(), 1));
        let entries = RwLock::new(BTreeMap::from([(
            "fixture".into(),
            Entry {
                bundle,
                state: State::Ready(Capabilities {
                    streaming_text: true,
                    model_list: true,
                }),
                operation: 1,
            },
        )]));
        observe_failure(
            &entries,
            "fixture",
            &ticket,
            &Error::new(ErrorKind::ProviderFailure),
        );
        assert!(matches!(
            entries.read().unwrap()["fixture"].state,
            State::Ready(_)
        ));
        assert!(
            matches!(&entries.read().unwrap()["fixture"].state, State::Ready(c) if c.model_list)
        );
        entries
            .write()
            .unwrap()
            .get_mut("fixture")
            .unwrap()
            .operation = 2;
        observe_failure(
            &entries,
            "fixture",
            &ticket,
            &Error::new(ErrorKind::ExternalFailure),
        );
        assert!(matches!(
            entries.read().unwrap()["fixture"].state,
            State::Ready(_)
        ));
        let current = Some((revision, 2));
        observe_failure(
            &entries,
            "fixture",
            &current,
            &Error::new(ErrorKind::ExternalFailure),
        );
        assert!(matches!(
            entries.read().unwrap()["fixture"].state,
            State::Failed(_)
        ));
    }
}

#[cfg(test)]
mod request_failure_integration_tests {
    use super::*;
    use futures_util::StreamExt;
    struct Permit;
    impl Authorization for Permit {
        fn check(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    fn invocation(
        connection_id: &str,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        ene_core::ports::llm::test_support::InvocationFixture,
    ) {
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        (
            cancel,
            ene_core::ports::llm::test_support::InvocationFixture {
                id: uuid::Uuid::new_v4().to_string(),
                connection: Connection {
                    id: connection_id.into(),
                    kind: "llm".into(),
                    adapter: "fixture".into(),
                    endpoint: "https://example.com/v1".into(),
                    options: serde_json::json!({}),
                    credential_state: "missing".into(),
                    capabilities: None,
                    last_check: None,
                },
                request: Some(Generation::for_test(
                    "https://example.com/v1".into(),
                    serde_json::json!({}),
                    "m".into(),
                    String::new(),
                    vec![],
                    128,
                )),
                operation: ene_core::domain::OperationKind::Conversation,
                secret: None,
                authorization: std::sync::Arc::new(Permit),
                cancel: receiver,
            },
        )
    }
    #[tokio::test]
    #[ignore = "requires OS Python, bubblewrap, and a compatible ene-provider extension"]
    async fn request_errors_do_not_disable_plugin_for_other_connections_or_model_requests() {
        assert!(
            std::path::Path::new("/usr/bin/python3").exists(),
            "OS Python is required inside the sandbox"
        );
        let root = crate::test_support::package_directory();
        let path = root.path().join("fixture");
        crate::test_support::create_directory(&path).unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        crate::test_support::write(path.join("manifest.json"), manifest.to_string()).unwrap();
        let executable = std::env::current_exe().unwrap();
        let target = executable
            .ancestors()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name == "debug" || name == "release")
            })
            .unwrap();
        let extension = std::env::var_os("ENE_PROVIDER_EXTENSION")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| target.join("libene_provider.so"));
        std::fs::copy(extension, path.join("ene_provider.so"))
            .expect("build the Python SDK before testing isolated Python providers");
        crate::test_support::write(
            path.join("provider.py"),
            r#"import asyncio,sys
sys.path.insert(0, '/plugin')
import ene_provider
class Provider:
 async def generate(self, request, broker, deltas):
  raise RuntimeError('upstream request failed')
 async def list_models(self, endpoint, options, broker):
  raise RuntimeError('upstream request failed')
async def main():
 await ene_provider.serve(Provider())
asyncio.run(main())
"#,
        )
        .unwrap();
        let registry = Registry {
            plugins: None,
            root: root.path().to_path_buf(),
            entries: std::sync::Arc::new(RwLock::new(BTreeMap::new())),
            supervisor: process::Supervisor::default(),
            discovery_errors: RwLock::new(Vec::new()),
        };
        registry.scan().unwrap();
        registry.enable("fixture".into(), true).await.unwrap();
        let (_cancel, mut oversized) = invocation("large");
        oversized.request = Some(Generation::for_test(
            "https://example.com/v1".into(),
            serde_json::json!({}),
            "m".into(),
            String::new(),
            vec![ChatMessage {
                role: ene_core::ports::llm::ChatRole::User,
                text: "a".repeat(1_100_000),
            }],
            128,
        ));
        let binding = registry.resolve(&oversized.connection).unwrap();
        let mut stream = oversized.build(binding).generate().await.unwrap();
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind, ErrorKind::ProviderFailure);
        assert!(stream.next().await.is_none());
        assert!(registry.ready("fixture").is_ok());
        for connection in ["first", "second"] {
            let (_cancel, input) = invocation(connection);
            let binding = registry.resolve(&input.connection).unwrap();
            let mut stream = input.build(binding).generate().await.unwrap();
            assert_eq!(
                stream.next().await.unwrap().unwrap_err().kind,
                ErrorKind::ProviderFailure
            );
            assert!(stream.next().await.is_none());
            let descriptor = registry
                .descriptors()
                .into_iter()
                .find(|d| d.id == "fixture")
                .unwrap();
            assert_eq!(descriptor.state, "ready");
            assert!(descriptor.capabilities.streaming_text && descriptor.capabilities.model_list);
        }
        let (_cancel, mut input) = invocation("third");
        input.operation = ene_core::domain::OperationKind::ModelList;
        input.request = None;
        let binding = registry.resolve(&input.connection).unwrap();
        assert_eq!(
            input.build(binding).models().await.unwrap_err().kind,
            ErrorKind::ProviderFailure
        );
        assert_eq!(registry.ready("fixture").unwrap().manifest.id, "fixture");
        registry.shutdown().await;
    }
}

fn diagnostic(path: &std::path::Path, error: &str) -> Descriptor {
    Descriptor {
        id: format!("invalid-package:{}", path.display()),
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        kind: "plugin".into(),
        endpoint: String::new(),
        options: serde_json::json!({}),
        settings: vec![],
        requires_credentials: false,
        environment_variable: None,
        destinations: vec![],
        state: "failed".into(),
        error: Some(error.into()),
        revision: String::new(),
        capabilities: Capabilities::default(),
        models: vec![],
    }
}

#[cfg(test)]
mod review_tests;
