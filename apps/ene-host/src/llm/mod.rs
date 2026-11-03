//! Provider-neutral text generation and outbound authorization.
use crate::domain::{
    Connection, Error, ErrorKind, MAX_MODEL_ID_BYTES, MAX_MODELS, OperationKind, validate_model_id,
};
use crate::ports::Result;
pub use ene_rpc::{
    Model, Provider as Descriptor, ProviderCapabilities as Capabilities, SettingField as Field,
};
use futures_util::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
}
impl ChatRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Generation {
    pub(crate) endpoint: String,
    pub(crate) options: Value,
    pub(crate) model: String,
    pub(crate) preamble: String,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) max_tokens: u64,
}
impl Generation {
    pub fn new(
        connection: &Connection,
        model: String,
        preamble: String,
        messages: Vec<ChatMessage>,
        max_tokens: u64,
    ) -> Result<Self> {
        validate_model_id(&model)?;
        if max_tokens == 0
            || messages.is_empty()
            || messages.last().is_none_or(|m| m.role != ChatRole::User)
            || messages.iter().enumerate().any(|(i, m)| {
                m.role
                    != if i % 2 == 0 {
                        ChatRole::User
                    } else {
                        ChatRole::Assistant
                    }
            })
        {
            return Err(Error::invalid());
        }
        let mut request = Self {
            endpoint: connection.endpoint.clone(),
            options: connection
                .options
                .as_option()
                .map(|options| serde_json::from_str(&options.json))
                .transpose()
                .map_err(|_| Error::invalid())?
                .unwrap_or_else(|| serde_json::json!({})),
            model,
            preamble,
            messages,
            max_tokens,
        };
        request.bound_history()?;
        Ok(request)
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn options(&self) -> &Value {
        &self.options
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn preamble(&self) -> &str {
        &self.preamble
    }
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }
    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }
}

#[derive(Clone)]
pub struct ProviderBinding {
    provider: Arc<dyn LlmProvider>,
    policy: Policy,
}
impl ProviderBinding {
    /// Registry resolves a concrete implementation and its policy together.
    pub fn resolve(provider: Arc<dyn LlmProvider>, connection: &Connection) -> Result<Self> {
        let mut policy = provider.policy(connection)?;
        policy = policy.bind_connection(connection);
        Ok(Self { provider, policy })
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
}
mod budget;
mod invocation;
pub(crate) use invocation::InvocationInput;
pub(crate) use invocation::Outbound;
pub use invocation::{
    AuthorizedTransport, CredentialPlacement, Invocation, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    Response,
};
#[cfg(test)]
pub(crate) mod test_support;
pub use budget::MAX_GENERATION_BYTES;
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
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
/// Every builtin and plugin adapter implements this same provider-neutral boundary.
pub trait LlmProvider: Send + Sync {
    fn policy(&self, connection: &Connection) -> Result<Policy>;
    fn generate(
        &self,
        invocation: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>>;
    fn models(&self, invocation: Invocation) -> BoxFuture<'_, Result<Vec<Model>>>;
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    struct MinimalProvider;
    impl LlmProvider for MinimalProvider {
        fn policy(&self, _: &Connection) -> Result<Policy> {
            Ok(Policy {
                revision: "same".into(),
                fingerprint: Value::Null,
                destinations: vec!["https://example.test".into()],
                requires_credentials: false,
                environment_variable: None,
            })
        }
        fn generate(
            &self,
            _: Invocation,
        ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
            Box::pin(async { Err(Error::unavailable()) })
        }
        fn models(&self, _: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
            Box::pin(async { Ok(vec![]) })
        }
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
            ProviderBinding::resolve(Arc::new(MinimalProvider), connection)
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
        let binding = ProviderBinding::resolve(Arc::new(MinimalProvider), &connection).unwrap();
        let original = Arc::new(Permission(AtomicBool::new(true)));
        let runtime = Arc::new(Permission(AtomicBool::new(true)));
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let invocation = Invocation::new(
            "i".into(),
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
