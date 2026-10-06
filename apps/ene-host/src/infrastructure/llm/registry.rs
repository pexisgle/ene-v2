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
        entry.state = "failed".into();
        entry.capabilities = Capabilities::default();
        entry.error =
            Some("プラグインの起動または通信に失敗しました。再度有効化してください".into());
    }
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
    #[cfg(test)]
    fn ready(&self, id: &str) -> Result<Bundle> {
        let entries = self.entries.read().map_err(|_| Error::unavailable())?;
        let e = entries
            .get(id)
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
        if e.state != "ready" {
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
            v.extend(entries.values().map(|e| {
                e.bundle
                    .descriptor(&e.state, e.capabilities.clone(), e.error.clone())
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
    fn resolve(&self, c: &Connection) -> Result<ProviderBinding> {
        if c.adapter == openai::ID {
            return ProviderBinding::resolve(std::sync::Arc::new(openai::Provider), c);
        }
        let entries = self.entries.read().map_err(|_| Error::unavailable())?;
        let entry = entries
            .get(&c.adapter)
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
        if entry.state != "ready" {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_failure_keeps_plugin_ready_and_old_process_cannot_poison_reenabled_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        std::fs::write(dir.path().join("manifest.json"), manifest.to_string()).unwrap();
        let bundle = manifest::load(dir.path()).unwrap();
        let revision = bundle.revision.clone();
        let ticket = Some((revision.clone(), 1));
        let entries = RwLock::new(BTreeMap::from([(
            "fixture".into(),
            Entry {
                bundle,
                state: "ready".into(),
                capabilities: Capabilities {
                    streaming_text: true,
                    model_list: true,
                },
                error: None,
                operation: 1,
                initialization: None,
            },
        )]));
        observe_failure(
            &entries,
            "fixture",
            &ticket,
            &Error::new(ErrorKind::ProviderFailure),
        );
        assert_eq!(entries.read().unwrap()["fixture"].state, "ready");
        assert!(entries.read().unwrap()["fixture"].capabilities.model_list);
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
        assert_eq!(entries.read().unwrap()["fixture"].state, "ready");
        let current = Some((revision, 2));
        observe_failure(
            &entries,
            "fixture",
            &current,
            &Error::new(ErrorKind::ExternalFailure),
        );
        assert_eq!(entries.read().unwrap()["fixture"].state, "failed");
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
    async fn request_errors_do_not_disable_plugin_for_other_connections_or_model_requests() {
        if !std::path::Path::new("/usr/bin/bwrap").exists() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture");
        std::fs::create_dir(&path).unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"endpoint":"https://example.com/v1","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        std::fs::write(path.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(path.join("provider.py"),r#"import sys,json
m=json.loads(sys.stdin.readline())
print(json.dumps({'type':'initialized','request_id':m['request_id'],'protocol_version':1,'capabilities':{'streaming_text':True,'model_list':True}}),flush=True)
for line in sys.stdin:
 m=json.loads(line)
 if m['type'] in ('generate','list_models'):
  print(json.dumps({'type':'error','request_id':m['request_id'],'code':'rate_limit','message':'upstream request failed'}),flush=True)
 elif m['type']=='shutdown':break
"#).unwrap();
        let registry = Registry {
            root: root.path().to_path_buf(),
            entries: std::sync::Arc::new(RwLock::new(BTreeMap::new())),
            supervisor: process::Supervisor::default(),
            discovery_error: RwLock::new(None),
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
                text: "a".repeat(ene_protocol::provider::MAX_LINE),
            }],
            128,
        ));
        let binding = registry.resolve(&oversized.connection).unwrap();
        let mut stream = oversized.build(binding).generate().await.unwrap();
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.reason, Some("input_limit"));
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
