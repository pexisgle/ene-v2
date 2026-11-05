//! Internal fixtures for hostile adapter/protocol contract tests.
use super::*;
use crate::ports::Secret;
use tokio::sync::watch;

/// A provider that answers no request; policy and transport contracts use it.
pub struct TestProvider(pub Policy);
impl LlmProvider for TestProvider {
    fn policy(&self, _: &Connection) -> Result<Policy> {
        Ok(self.0.clone())
    }
    fn generate(&self, _: Invocation) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
        Box::pin(async { Err(Error::unavailable()) })
    }
    fn models(&self, _: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

pub struct InvocationFixture {
    pub connection: Connection,
    pub request: Option<Generation>,
    pub secret: Option<Secret>,
    pub authorization: Arc<dyn Authorization>,
    pub cancel: watch::Receiver<bool>,
    pub operation: OperationKind,
}
impl InvocationFixture {
    pub fn build(self, binding: ProviderBinding) -> Invocation {
        let input = match self.operation {
            OperationKind::Conversation => {
                InvocationInput::ConnectionCheck(self.request.unwrap_or_else(|| {
                    Generation::for_test(
                        self.connection.endpoint.clone(),
                        self.connection
                            .options
                            .as_option()
                            .map(|options| {
                                serde_json::from_str(&options.json).expect("fixture options")
                            })
                            .unwrap_or_else(|| serde_json::json!({})),
                        "fixture".into(),
                        String::new(),
                        vec![],
                        128,
                    )
                }))
            }
            OperationKind::ConnectionCheck => {
                InvocationInput::ConnectionCheck(self.request.expect("check fixture generation"))
            }
            OperationKind::ModelList => InvocationInput::ModelList,
            OperationKind::Task | OperationKind::Consolidation => {
                InvocationInput::Task(self.request.expect("task fixture generation"))
            }
            OperationKind::Embedding => InvocationInput::Embedding(EmbeddingRequest {
                model: "fixture".into(),
                input: vec!["fixture".into()],
            }),
        };
        Invocation::new(
            self.connection,
            input,
            self.secret,
            self.authorization,
            self.cancel,
            binding,
        )
    }
    pub fn with_destinations(self, destinations: Vec<String>) -> Invocation {
        let policy = Policy {
            revision: "fixture".into(),
            fingerprint: serde_json::json!({"fixture":true}),
            destinations,
            requires_credentials: self.secret.is_some(),
            environment_variable: None,
        };
        let binding = ProviderBinding::resolve(Arc::new(TestProvider(policy)), &self.connection)
            .expect("fixture policy");
        self.build(binding)
    }
}
impl Generation {
    /// Allows malformed payloads only for testing the next protocol boundary.
    pub fn for_test(
        endpoint: String,
        options: Value,
        model: String,
        preamble: String,
        messages: Vec<ChatMessage>,
        max_tokens: u64,
    ) -> Self {
        Self {
            endpoint,
            options,
            model,
            preamble,
            messages,
            max_tokens,
        }
    }
}
