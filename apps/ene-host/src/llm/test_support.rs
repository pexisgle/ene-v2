//! Internal fixtures for hostile adapter/protocol contract tests.
use super::*;
use crate::ports::Secret;
use tokio_util::sync::CancellationToken;

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
    pub request: Option<rig_core::completion::CompletionRequest>,
    pub secret: Option<Secret>,
    pub authorization: Arc<dyn Authorization>,
    pub cancel: CancellationToken,
    pub operation: OperationKind,
}
impl InvocationFixture {
    pub fn build(self, binding: ProviderBinding) -> Invocation {
        let input = match self.operation {
            OperationKind::Conversation => InvocationInput::ConnectionCheck(
                self.request
                    .unwrap_or_else(|| completion_for_test("fixture", 128)),
            ),
            OperationKind::ConnectionCheck => {
                InvocationInput::ConnectionCheck(self.request.expect("check fixture generation"))
            }
            OperationKind::ModelList => InvocationInput::ModelList,
            OperationKind::Task | OperationKind::Consolidation => {
                InvocationInput::Task(self.request.expect("task fixture generation"))
            }
            OperationKind::Embedding => InvocationInput::Embedding(vec!["fixture".into()]),
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

pub fn completion_for_test(
    model: &str,
    max_tokens: u64,
) -> rig_core::completion::CompletionRequest {
    let mut request =
        rig_core::completion::CompletionRequest::new("fixture").max_tokens(max_tokens);
    request.model = Some(model.into());
    request
}
