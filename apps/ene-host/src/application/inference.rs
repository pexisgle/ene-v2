//! Task inference: the host owns the connection, credentials and grant checks.
use super::generation::{authorize, config, execute, invocation, register};
use super::*;
use crate::{
    domain::rules,
    llm::{ChatMessage, ChatRole, Embedding, EmbeddingRequest, Event, Generation, InvocationInput},
    ports::inference::{InferenceRequest, TaskInference},
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture};
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

/// The guest may ask for a long answer, but the host bounds the budget.
const MAX_TASK_TOKENS: u64 = 8192;
/// The one embedding model the built-in OpenAI adapter knows how to price and
/// store. Its version is part of the vector index key.
pub(crate) const EMBEDDING_MODEL: &str = "text-embedding-3-small";
const MAX_EMBEDDING_INPUTS: usize = 32;
const MAX_EMBEDDING_CHARS: usize = 16_000;

pub(crate) struct Inference {
    weak: Weak<Inner>,
}

impl TaskInference for Inference {
    fn complete<'a>(
        &'a self,
        request: InferenceRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String, Error>> {
        async move {
            let Some(inner) = self.weak.upgrade() else {
                return Err(Error::unavailable());
            };
            HostApplication(inner).infer_task(request, cancel).await
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
        request: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<String, Error> {
        if request.messages.is_empty() {
            return Err(Error::invalid());
        }
        let max_tokens = request.max_tokens.clamp(1, MAX_TASK_TOKENS);
        let id = Uuid::new_v4().to_string();
        let mut controls = self.0.controls.lock().await;
        if controls.failed() {
            return Err(Error::unavailable());
        }
        let configuration = config(self, &mut controls).await?;
        let companion = configuration
            .companions
            .get(&request.companion_id)
            .ok_or_else(Error::invalid)?;
        // Tasks use the dedicated assignment; falling back to the companion
        // model would silently widen what the user configured.
        let assignment = companion
            .model_assignments
            .task
            .as_option()
            .ok_or_else(model_unset)?;
        let connection =
            rules::connection(&configuration, &assignment.connection_id).ok_or_else(model_unset)?;
        let context = authorize(
            self,
            &configuration,
            &controls,
            &request.companion_id,
            &connection,
            OperationKind::Task,
        )?;
        let messages = request
            .messages
            .into_iter()
            .map(|(user, text)| ChatMessage {
                role: if user {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                },
                text,
            })
            .collect();
        let generation = Generation::new(
            &connection,
            assignment.model_id.clone(),
            request.preamble,
            messages,
            max_tokens,
        )?;
        let receiver = register(
            &mut controls,
            id.clone(),
            request.companion_id.clone(),
            &connection,
            &context,
            OperationKind::Task,
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
            InvocationInput::Task(generation),
            receiver,
        );
        let output_bytes = OperationKind::Task.spec().output_bytes;
        let work = async move {
            let mut stream = input.generate().await?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event? {
                    Event::Delta(delta) => {
                        if text.len() + delta.len() > output_bytes {
                            return Err(Error::new(ErrorKind::ResourceLimit));
                        }
                        text.push_str(&delta);
                    }
                    Event::Finished(_) => break,
                }
            }
            Ok(text)
        };
        let result = execute(self, OperationKind::Task, &mut cancelled, work).await;
        forward.abort();
        result
    }
}

impl HostApplication {
    /// Embed index text under the companion's task assignment and task grant.
    /// The index stores the model and dimension with each vector; a different
    /// adapter or model must not share that space.
    pub(crate) async fn embed_task(
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
            .task
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
