//! Provider-neutral text generation and outbound authorization.
use crate::domain::{
    Connection, Error, ErrorKind, MAX_MODEL_ID_BYTES, MAX_MODELS, OperationKind, validate_model_id,
};
use crate::ports::Result;
pub use ene_rpc::{Model, Provider as Descriptor, ProviderCapabilities as Capabilities};
use futures_util::{future::BoxFuture, stream::BoxStream};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct Policy {
    /// Same implementation revision as the public provider descriptor.
    pub revision: String,
    pub fingerprint: Value,
    pub destinations: Vec<String>,
    pub requires_credentials: bool,
    pub environment_variable: Option<String>,
}
impl Policy {
    pub fn bind_connection(mut self, connection: &Connection) -> Self {
        // Scope identity is host-owned. Adapters may add restrictions but
        // cannot omit connection settings or destinations from revocation.
        self.fingerprint = serde_json::json!({
            "contract": "ene-policy-v1", "kind": connection.kind,
            "adapter": connection.adapter, "endpoint": connection.endpoint,
            "options": connection.options.as_option()
                .map(|options| serde_json::from_str::<Value>(&options.json).unwrap_or(Value::Null))
                .unwrap_or_else(|| serde_json::json!({})),
            "provider_revision": self.revision,
            "destinations": self.destinations, "requires_credentials": self.requires_credentials,
            "environment_variable": self.environment_variable, "provider": self.fingerprint,
        });
        self
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Embedding {
    pub vectors: Vec<Vec<f32>>,
    pub input_tokens: u64,
}

/// Native Rig provider routes. Only the built-in registry can mark a binding
/// native, so injected test providers and Wasm guests keep their own behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeWire {
    OpenAiResponses,
    OpenRouterChat,
}
#[derive(Clone)]
pub struct ProviderBinding {
    provider: Arc<dyn LlmProvider>,
    policy: Policy,
    native: Option<NativeWire>,
}
impl ProviderBinding {
    /// Registry resolves a concrete implementation and its policy together.
    pub fn resolve(provider: Arc<dyn LlmProvider>, connection: &Connection) -> Result<Self> {
        let mut policy = provider.policy(connection)?;
        policy = policy.bind_connection(connection);
        Ok(Self {
            provider,
            policy,
            native: None,
        })
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub(crate) fn with_native(mut self, wire: NativeWire) -> Self {
        self.native = Some(wire);
        self
    }
    pub(crate) fn native(&self) -> Option<NativeWire> {
        self.native
    }
    pub(crate) fn provider(&self) -> Arc<dyn LlmProvider> {
        self.provider.clone()
    }
}
mod invocation;
pub(crate) use invocation::InvocationInput;
pub(crate) use invocation::Outbound;
pub use invocation::{AuthorizedTransport, CredentialPlacement, Invocation, Response};
#[cfg(test)]
pub(crate) mod test_support;
#[derive(Clone, Debug)]
pub enum Event {
    Delta(String),
    Finished(String),
}
pub trait Authorization: Send + Sync {
    fn check(&self) -> BoxFuture<'_, Result<()>>;
}

pub fn validate_models(models: &[Model]) -> Result<()> {
    if models.len() > MAX_MODELS
        || models
            .iter()
            .any(|m| validate_model_id(&m.id).is_err() || m.name.len() > MAX_MODEL_ID_BYTES)
    {
        return Err(Error::new(ErrorKind::ExternalFailure));
    }
    Ok(())
}

pub trait ProviderRegistry: Send + Sync {
    fn descriptors(&self) -> Vec<Descriptor>;
    fn resolve(&self, connection: &Connection) -> Result<ProviderBinding>;
    /// Verified authorization scope, independent of whether execution is ready.
    fn policy(&self, connection: &Connection) -> Result<Policy> {
        Ok(self.resolve(connection)?.policy().clone())
    }
    /// A changed implementation may no longer support the previously saved connection.
    fn scope_fingerprint(&self, connection: &Connection) -> Result<Value> {
        self.policy(connection).map(|p| p.fingerprint)
    }
    fn reload(&self) -> BoxFuture<'_, Result<()>>;
    fn enable(&self, id: String, enabled: bool) -> BoxFuture<'_, Result<()>>;
}
/// Every builtin and plugin adapter implements this same provider-neutral boundary.
pub trait LlmProvider: Send + Sync {
    fn policy(&self, connection: &Connection) -> Result<Policy>;
    /// Guest-specific text stream. Native OpenAI/OpenRouter use Rig directly.
    fn generate(&self, _: Invocation) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
        Box::pin(async { Err(Error::new(ErrorKind::UnsupportedCapability)) })
    }
    fn models(&self, invocation: Invocation) -> BoxFuture<'_, Result<Vec<Model>>>;
    fn embed(&self, _: Invocation) -> BoxFuture<'_, Result<crate::llm::Embedding>> {
        Box::pin(async { Err(Error::new(ErrorKind::UnsupportedCapability)) })
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::llm::test_support::TestProvider;
    fn minimal_provider() -> TestProvider {
        TestProvider(Policy {
            revision: "same".into(),
            fingerprint: Value::Null,
            destinations: vec!["https://example.test".into()],
            requires_credentials: false,
            environment_variable: None,
        })
    }
    #[test]
    fn host_scope_changes_even_when_an_adapter_omits_its_fingerprint() {
        let mut connection = Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: "minimal".into(),
            endpoint: "https://example.test/v1".into(),
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            credential_state: "missing".into(),
            capabilities: Default::default(),
            last_check: Default::default(),
            ..Default::default()
        };
        let policy = |connection: &Connection| {
            ProviderBinding::resolve(Arc::new(minimal_provider()), connection)
                .unwrap()
                .policy()
                .fingerprint
                .clone()
        };
        let original = policy(&connection);
        assert_eq!(original["options"], serde_json::json!({}));
        connection.options = ene_rpc::JsonDocument {
            json: " { } ".into(),
            ..Default::default()
        }
        .into();
        assert_eq!(
            policy(&connection),
            original,
            "JSON formatting does not change consent"
        );
        connection.credential_state = "stored".into();
        connection.last_check = ene_rpc::JsonDocument {
            json: serde_json::json!({"success":true}).to_string(),
            ..Default::default()
        }
        .into();
        assert_eq!(
            policy(&connection),
            original,
            "operational state is separate from consent scope"
        );
        connection.options = ene_rpc::JsonDocument {
            json: serde_json::json!({"new_route":true}).to_string(),
            ..Default::default()
        }
        .into();
        assert_ne!(policy(&connection), original);
        connection.options = ene_rpc::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into();
        connection.endpoint = "https://example.test/other".into();
        assert_ne!(policy(&connection), original);
    }

    #[tokio::test]
    async fn adding_runtime_scope_cannot_replace_original_send_authorization() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Permission(AtomicBool);
        impl Authorization for Permission {
            fn check(&self) -> BoxFuture<'_, Result<()>> {
                Box::pin(async {
                    if self.0.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(Error::new(ErrorKind::PermissionRequired))
                    }
                })
            }
        }
        let connection = Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: "minimal".into(),
            endpoint: "https://example.test/v1".into(),
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            credential_state: "missing".into(),
            capabilities: Default::default(),
            last_check: Default::default(),
            ..Default::default()
        };
        let binding = ProviderBinding::resolve(Arc::new(minimal_provider()), &connection).unwrap();
        let original = Arc::new(Permission(AtomicBool::new(true)));
        let runtime = Arc::new(Permission(AtomicBool::new(true)));
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let invocation = Invocation::new(
            connection,
            InvocationInput::ModelList,
            None,
            original.clone(),
            receiver,
            binding,
        )
        .restrict(runtime.clone());
        invocation.authorization().check().await.unwrap();
        original.0.store(false, Ordering::SeqCst);
        assert_eq!(
            invocation.authorization().check().await.unwrap_err().kind,
            ErrorKind::PermissionRequired
        );
        original.0.store(true, Ordering::SeqCst);
        runtime.0.store(false, Ordering::SeqCst);
        assert_eq!(
            invocation.authorization().check().await.unwrap_err().kind,
            ErrorKind::PermissionRequired
        );
    }
}
