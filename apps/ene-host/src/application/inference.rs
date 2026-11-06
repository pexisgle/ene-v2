//! Task inference: the host owns the connection, credentials and grant checks.
use super::generation::{authorize, config, execute, invocation, register};
use super::*;
use crate::{
    domain::rules,
    llm::{Event, InvocationInput},
    ports::inference::TaskInference,
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture};
use rig_core::completion::CompletionRequest;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

/// The guest may ask for a long answer, but the host bounds the budget.
const MAX_TASK_TOKENS: u64 = 8192;
/// The one embedding model the built-in OpenAI adapter knows how to price and
/// store. Its version is part of the vector index key.
/// The structured output of a host memory call. The finish reason is kept so
/// a truncated reply is not parsed as a valid no-change decision.
pub(crate) struct InferenceOutput {
    pub text: String,
    pub finish: String,
}
const MAX_EMBEDDING_INPUTS: usize = 32;
const MAX_EMBEDDING_CHARS: usize = 16_000;

pub(crate) struct Inference {
    weak: Weak<Inner>,
}

impl TaskInference for Inference {
    fn complete<'a>(
        &'a self,
        companion_id: String,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, Error>> {
        async move {
            let Some(inner) = self.weak.upgrade() else {
                return Err(Error::unavailable());
            };
            HostApplication(inner)
                .infer_task(companion_id, request, cancel)
                .await
        }
        .boxed()
    }
}

/// Removes the running entry even when the awaiting service future is dropped
/// by the runtime's cancellation or timeout path.
struct RunGuard {
    app: HostApplication,
    id: String,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut controls) = self.app.0.controls.try_lock() {
            controls.running.remove(&self.id);
        }
    }
}

impl HostApplication {
    pub(crate) fn inference(&self) -> Arc<dyn TaskInference> {
        Arc::new(Inference {
            weak: Arc::downgrade(&self.0),
        })
    }

    pub(crate) async fn infer_task(
        &self,
        companion_id: String,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<String, Error> {
        self.infer(companion_id, request, cancel, false)
            .await
            .map(|output| output.text)
    }

    /// Memory consolidation is its own host purpose: it uses the companion's
    /// configured connection and a memory grant, not the task assignment.
    pub(crate) async fn infer_memory(
        &self,
        companion_id: String,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceOutput, Error> {
        self.infer(companion_id, request, cancel, true).await
    }

    async fn infer(
        &self,
        companion_id: String,
        mut request: CompletionRequest,
        cancel: CancellationToken,
        memory: bool,
    ) -> Result<InferenceOutput, Error> {
        request
            .validate_message_content()
            .map_err(|_| Error::invalid())?;
        let operation = if memory {
            OperationKind::Consolidation
        } else {
            OperationKind::Task
        };
        let max_tokens = request
            .max_tokens
            .unwrap_or(MAX_TASK_TOKENS)
            .clamp(1, MAX_TASK_TOKENS);
        let id = Uuid::new_v4().to_string();
        let mut controls = self.0.controls.lock().await;
        if controls.failed() {
            return Err(Error::unavailable());
        }
        let configuration = config(self, &mut controls).await?;
        let companion = configuration
            .companions
            .get(&companion_id)
            .ok_or_else(Error::invalid)?;
        // Plugin tasks use the task slot; memory processing uses the
        // companion connection under a separate memory purpose and grant.
        let assignment = if memory {
            companion.model_assignments.companion.as_option()
        } else {
            companion.model_assignments.task.as_option()
        }
        .ok_or_else(model_unset)?;
        let connection =
            rules::connection(&configuration, &assignment.connection_id).ok_or_else(model_unset)?;
        let context = authorize(
            self,
            &configuration,
            &controls,
            &companion_id,
            &connection,
            operation,
        )?;
        // The host chooses the permitted model, regardless of a caller override.
        // Keep Rig's full content, tools and provenance rather than flattening.
        request.model = Some(assignment.model_id.clone());
        request.max_tokens = Some(max_tokens);
        const MAX_TASK_REQUEST_BYTES: usize = 256 * 1024;
        if serde_json::to_vec(&request)
            .map_err(|_| Error::invalid())?
            .len()
            > MAX_TASK_REQUEST_BYTES
        {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("input_limit"),
            });
        }
        // The run's token is a child of the caller's, so the runtime dropping
        // the future and the host revoking the run both stop the outbound call.
        let cancelled = register(
            &mut controls,
            id.clone(),
            companion_id.clone(),
            &connection,
            &context,
            operation,
            Some(&cancel),
        );
        drop(controls);
        let _guard = RunGuard {
            app: self.clone(),
            id: id.clone(),
        };
        let input = invocation(
            self,
            connection,
            context,
            id,
            InvocationInput::Task(request),
            cancelled.clone(),
        );
        let output_bytes = operation.spec().output_bytes;
        let work = async move {
            let mut text = String::new();
            let finish = if input.is_native() {
                // Rig decodes the native stream; no host event translation.
                use rig_core::streaming::{Item, StreamEvent};
                let mut stream = input.native_stream().await?;
                while let Some(item) = stream.next().await {
                    let item = item.map_err(|_| Error::new(ErrorKind::ProviderFailure))?;
                    if let Item::Event(StreamEvent::Text { text: delta, .. }) = item {
                        if text.len() + delta.len() > output_bytes {
                            return Err(Error::new(ErrorKind::ResourceLimit));
                        }
                        text.push_str(&delta);
                    }
                }
                let response = stream
                    .finish()
                    .await
                    .map_err(|_| Error::new(ErrorKind::ProviderFailure))?;
                if matches!(
                    response.finish_reason(),
                    Some(rig_core::completion::FinishReason::Stop)
                ) {
                    "stop".to_owned()
                } else {
                    "incomplete".to_owned()
                }
            } else {
                // The separate Wasm guest ABI remains explicitly text-only.
                let mut stream = input.generate().await?;
                let mut finish = String::new();
                while let Some(event) = stream.next().await {
                    match event? {
                        Event::Delta(delta) => {
                            if text.len() + delta.len() > output_bytes {
                                return Err(Error::new(ErrorKind::ResourceLimit));
                            }
                            text.push_str(&delta);
                        }
                        Event::Finished(reason) => {
                            finish = reason;
                            break;
                        }
                    }
                }
                finish
            };
            Ok(InferenceOutput { text, finish })
        };
        execute(self, operation, &cancelled, work).await
    }
}

impl HostApplication {
    /// Embed index text under the companion's task assignment and task grant.
    /// The index stores the model and dimension with each vector; a different
    /// adapter or model must not share that space.
    pub(crate) async fn embed_memory(
        &self,
        companion_id: &str,
        input: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, Error> {
        if input.is_empty()
            || input.len() > MAX_EMBEDDING_INPUTS
            || input
                .iter()
                .any(|text| text.is_empty() || text.chars().count() > MAX_EMBEDDING_CHARS)
        {
            return Err(Error::invalid());
        }
        #[cfg(test)]
        if let Some(hook) = self.0.embedding_hook.get() {
            return hook(companion_id.to_owned(), input).await;
        }
        let id = Uuid::new_v4().to_string();
        let mut controls = self.0.controls.lock().await;
        if controls.failed() {
            return Err(Error::unavailable());
        }
        let configuration = config(self, &mut controls).await?;
        let companion = configuration
            .companions
            .get(companion_id)
            .ok_or_else(Error::invalid)?;
        let assignment = companion
            .model_assignments
            .companion
            .as_option()
            .ok_or_else(model_unset)?;
        let connection =
            rules::connection(&configuration, &assignment.connection_id).ok_or_else(model_unset)?;
        if connection.adapter != "openai" {
            return Err(Error::new(ErrorKind::UnsupportedCapability));
        }
        let context = authorize(
            self,
            &configuration,
            &controls,
            companion_id,
            &connection,
            OperationKind::Embedding,
        )?;
        let cancelled = register(
            &mut controls,
            id.clone(),
            companion_id.to_owned(),
            &connection,
            &context,
            OperationKind::Embedding,
            None,
        );
        drop(controls);
        let _guard = RunGuard {
            app: self.clone(),
            id: id.clone(),
        };
        let expected = input.len();
        let input = invocation(
            self,
            connection,
            context,
            id,
            InvocationInput::Embedding(input),
            cancelled.clone(),
        );
        let response = execute(self, OperationKind::Embedding, &cancelled, async move {
            input.embed().await
        })
        .await?;
        let _ = self
            .0
            .repository
            .mind_record_embedding(
                companion_id.to_owned(),
                crate::llm::EMBEDDING_MODEL.into(),
                response.usage.input_tokens.unwrap_or_default(),
                super::mind::now(),
            )
            .await;
        embedding_vectors(response, expected)
    }
}

/// The vectors of a response as the vector store keeps them: f64 becomes f32
/// here, once, and a shape the store could not line up with the inputs is
/// rejected instead of being written short.
fn embedding_vectors(
    response: rig_core::embeddings::EmbeddingResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, Error> {
    let vectors = response
        .embeddings
        .into_iter()
        .map(|embedding| {
            embedding
                .vec
                .into_iter()
                .map(|value| value as f32)
                .collect::<Vec<f32>>()
        })
        .collect::<Vec<_>>();
    let dimension = vectors.first().map(Vec::len).unwrap_or_default();
    if vectors.len() != expected
        || dimension == 0
        || vectors.iter().any(|vector| vector.len() != dimension)
    {
        return Err(Error::new(ErrorKind::ExternalFailure));
    }
    Ok(vectors)
}

fn model_unset() -> Error {
    Error {
        kind: ErrorKind::StateConflict,
        reason: Some("model_unset"),
    }
}
