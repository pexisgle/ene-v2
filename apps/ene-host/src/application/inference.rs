//! Task inference: the host owns the connection, credentials and grant checks.
use super::generation::{authorize, config, execute, invocation, register};
use super::*;
use crate::{
    domain::rules,
    llm::{Embedding, EmbeddingRequest, Event, InvocationInput},
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
pub(crate) const EMBEDDING_MODEL: &str = "text-embedding-3-small";
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
        let receiver = register(
            &mut controls,
            id.clone(),
            companion_id.clone(),
            &connection,
            &context,
            operation,
        );
        let mut cancelled = receiver.clone();
        let sender = controls
            .running
            .get(&id)
            .map(|running| running.cancel.clone())
            .ok_or_else(Error::unavailable)?;
        drop(controls);
        let _guard = RunGuard {
            app: self.clone(),
            id: id.clone(),
        };
        // The runtime drops the service future on cancellation; forward the
        // token into the provider's watch channel so the outbound call stops.
        let forward = tokio::spawn({
            let token = cancel.clone();
            async move {
                token.cancelled().await;
                let _ = sender.send(true);
            }
        });
        let input = invocation(
            self,
            connection,
            context,
            id,
            InvocationInput::Task(request),
            receiver,
        );
        let output_bytes = operation.spec().output_bytes;
        let work = async move {
            let mut text = String::new();
            let finish = if input.native_wire().is_some() {
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
        let result = execute(self, operation, &mut cancelled, work).await;
        forward.abort();
        result
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
    ) -> Result<Embedding, Error> {
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
        let receiver = register(
            &mut controls,
            id.clone(),
            companion_id.to_owned(),
            &connection,
            &context,
            OperationKind::Embedding,
        );
        let mut cancelled = receiver.clone();
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
            InvocationInput::Embedding(EmbeddingRequest {
                model: EMBEDDING_MODEL.into(),
                input,
            }),
            receiver,
        );
        let embedding = execute(self, OperationKind::Embedding, &mut cancelled, async move {
            input.embed().await
        })
        .await?;
        let _ = self
            .0
            .repository
            .mind_record_embedding(
                companion_id.to_owned(),
                EMBEDDING_MODEL.into(),
                embedding.input_tokens,
                super::mind::now(),
            )
            .await;
        Ok(embedding)
    }
}

fn model_unset() -> Error {
    Error {
        kind: ErrorKind::StateConflict,
        reason: Some("model_unset"),
    }
}
