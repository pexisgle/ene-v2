//! The parent conversation: acceptance, stop, history and the Rig agent's
//! model connection. The conversation itself is the kernel's.

use super::generation::{authorize, check_access, config, publish};
use super::*;
use crate::domain::{Error, ErrorKind, GrantPurpose, MAX_INPUT_BYTES, OperationKind, rules};
use crate::llm::{Authorization, Outbound, Policy};
use crate::ports::{Outcome, Receipt};
use crate::rig::{Event, Runtime};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use uuid::Uuid;

/// A test-only replacement for the provider connection, so a host test can
/// exercise acceptance and notifications with a scripted model.
#[cfg(test)]
pub(crate) type TestModel =
    dyn Fn(&str, Arc<Outbound>) -> crate::ports::Result<rig_ecs::model::Connection> + Send + Sync;

/// The authorization callback a kernel model call checks on every send: the
/// same configuration, credential and grant checks as the rest of the host.
struct ConversationAuthority {
    app: HostApplication,
    companion: String,
    connection: String,
    policy: Policy,
    nonce: Option<String>,
}

impl Authorization for ConversationAuthority {
    fn check(&self) -> BoxFuture<'_, crate::ports::Result<()>> {
        let app = self.app.clone();
        Box::pin(async move {
            let controls = app.0.controls.lock().await;
            check_access(
                &app,
                &controls,
                &self.companion,
                &self.connection,
                &self.policy,
                self.nonce.as_deref(),
                OperationKind::Conversation,
            )
        })
    }
}

/// What the connection depends on, as far as the kernel is concerned. A
/// change reconnects before the next input; the changed model, credential or
/// grant is never used for a later turn without it.
fn signature(
    configuration: &Configuration,
    companion: &str,
    model_config: &Companion,
    connection: &ProviderConnection,
    policy: &Policy,
    nonce: Option<&str>,
) -> String {
    let grants = configuration
        .grants
        .iter()
        .filter(|grant| {
            grant.companion_id == companion
                && grant.connection_id == connection.id
                && grant.purpose == GrantPurpose::Conversation
        })
        .map(|grant| {
            json!({
                "id": grant.id,
                "status": grant.status,
                "revision": grant.revision,
                "destinations": grant.destinations,
                "data_kinds": grant.data_kinds,
                "fingerprint": grant.connection_fingerprint,
            })
        })
        .collect::<Vec<_>>();
    let model = model_config
        .model_assignments
        .companion
        .as_option()
        .map(|assignment| assignment.model_id.as_str())
        .unwrap_or_default();
    json!({
        "companion": companion,
        "connection": connection.id,
        "adapter": connection.adapter,
        "endpoint": connection.endpoint,
        "model": model,
        "mind": model_config.mind,
        "policy": policy.fingerprint,
        "nonce": nonce,
        "grants": grants,
    })
    .to_string()
}

pub(crate) async fn accept(
    app: HostApplication,
    companion: String,
    key: String,
    request: MessageInput,
) -> crate::ports::Result<(Outcome, Value)> {
    if request.text.trim().is_empty() || request.text.len() > MAX_INPUT_BYTES {
        return Err(Error::invalid());
    }
    // The receipt keeps the historical body shape so requests accepted by an
    // older host still replay with the same key.
    let body = json!({"text":request.text,"resources":[],"question_id":null,"decision":null});
    let target = format!("/v1/companions/{companion}/messages");
    let request_id = Uuid::new_v4().to_string();
    let acceptance = MessageAcceptance {
        request_id: request_id.clone(),
        ..Default::default()
    };
    let response =
        serde_json::to_value(&acceptance).map_err(|_| Error::new(ErrorKind::StorageFailure))?;

    {
        let mut controls = app.0.controls.lock().await;
        if *app.0.shutdown.borrow() {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        if let Some((outcome, stored)) = app
            .0
            .repository
            .receipt(key.clone(), target.clone(), body.clone())
            .await?
        {
            return match outcome {
                Outcome::Accepted => Ok((Outcome::Accepted, stored)),
                // A pending receipt from a live request is not resolved by a
                // duplicate; a pending receipt after a restart is settled
                // from the kernel's saved conversation and inbox.
                Outcome::Pending if controls.inflight.contains(&key) => Err(Error {
                    kind: ErrorKind::Unavailable,
                    reason: Some("not_delivered"),
                }),
                Outcome::Pending => {
                    let request_id = stored
                        .get("requestId")
                        .or_else(|| stored.get("request_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| Error::new(ErrorKind::StorageFailure))?;
                    let found = app
                        .rig()?
                        .has_request(companion.clone(), request_id.clone())
                        .await?;
                    if found {
                        app.0
                            .repository
                            .complete_receipt(Receipt {
                                key: key.clone(),
                                target: target.clone(),
                                request: body.clone(),
                                outcome: Outcome::Accepted,
                                response: stored.clone(),
                            })
                            .await?;
                        return Ok((Outcome::Accepted, stored));
                    }
                    let failure = json!(ene_rpc::ApiFailure {
                        request_id,
                        error: Failure {
                            code: "unavailable".into(),
                            message: "ホストが中断しました".into(),
                            details: ene_rpc::JsonDocument {
                                json: json!({"reason":"not_delivered"}).to_string(),
                                ..Default::default()
                            }
                            .into(),
                            ..Default::default()
                        }
                        .into(),
                        http_status: 503,
                        ..Default::default()
                    });
                    app.0
                        .repository
                        .complete_receipt(Receipt {
                            key,
                            target,
                            request: body,
                            outcome: Outcome::Unavailable,
                            response: failure,
                        })
                        .await?;
                    Err(Error {
                        kind: ErrorKind::Unavailable,
                        reason: Some("not_delivered"),
                    })
                }
                Outcome::Unavailable => Err(Error {
                    kind: ErrorKind::Unavailable,
                    reason: Some("not_delivered"),
                }),
                _ => Err(Error::new(ErrorKind::StorageFailure)),
            };
        }
        controls.admit(&app)?;
        let configuration = config(&app, &mut controls).await?;
        let companion_config = configuration
            .companions
            .get(&companion)
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let assignment = companion_config
            .model_assignments
            .companion
            .as_option()
            .ok_or(Error {
                kind: ErrorKind::StateConflict,
                reason: Some("model_unset"),
            })?
            .clone();
        let connection = rules::connection(&configuration, &assignment.connection_id)
            .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
        if !crate::infrastructure::llm::can_serve_conversation(&connection) {
            return Err(Error {
                kind: ErrorKind::UnsupportedCapability,
                reason: Some("adapter_unsupported"),
            });
        }
        let context = authorize(
            &app,
            &configuration,
            &controls,
            &companion,
            &connection,
            OperationKind::Conversation,
        )?;
        let policy = context.binding.policy().clone();
        let nonce = context.secret.as_ref().map(|secret| secret.nonce.clone());
        let signature = signature(
            &configuration,
            &companion,
            companion_config,
            &connection,
            &policy,
            nonce.as_deref(),
        );
        let authority = Arc::new(ConversationAuthority {
            app: app.clone(),
            companion: companion.clone(),
            connection: connection.id.clone(),
            policy: policy.clone(),
            nonce: nonce.clone(),
        });
        let outbound = Arc::new(Outbound::new(
            connection.clone(),
            context.secret,
            authority,
            context.binding,
            OperationKind::Conversation,
        ));
        #[cfg(test)]
        let model = match app.0.test_model.get() {
            Some(factory) => factory(&assignment.model_id, outbound)?,
            None => crate::infrastructure::llm::agent_connection(outbound, &assignment.model_id)?,
        };
        #[cfg(not(test))]
        let model = crate::infrastructure::llm::agent_connection(outbound, &assignment.model_id)?;
        app.0
            .repository
            .reserve(Receipt {
                key: key.clone(),
                target: target.clone(),
                request: body.clone(),
                outcome: Outcome::Pending,
                response: response.clone(),
            })
            .await?;
        controls.inflight.insert(key.clone());
        if controls.connected.get(&companion) != Some(&signature) {
            // The connect must reach the kernel before any later input for
            // this companion, so a concurrent accept cannot deliver first.
            let runtime = app.rig()?;
            let prompt = app.mind_prompt(&companion, &companion_config.mind).await?;
            // Each root companion's tool calls are bound to that companion by
            // the kernel; there is no shared "current owner".
            let mind_tools = true;
            if let Err(error) = runtime.connect(companion.clone(), prompt, model, mind_tools) {
                let _ = app
                    .0
                    .repository
                    .release_receipt(key.clone(), target, body)
                    .await;
                controls.inflight.remove(&key);
                return Err(error);
            }
            controls.connected.insert(companion.clone(), signature);
        }
    }

    let runtime = app.rig()?;
    // The turn's model input gets a small automatic recall for the accepted
    // text. It is a best-effort view; a missing index never blocks the turn.
    if let Ok(core) = app.companion(&companion).await {
        let _ = app
            .mind_prompt_with_recall(&companion, &core.mind, &request.text, &request_id)
            .await;
    }
    if let Err(error) = runtime
        .deliver(companion.clone(), request.text.clone(), request_id.clone())
        .await
    {
        let _ = app.0.repository.release_receipt(key, target, body).await;
        let mut controls = app.0.controls.lock().await;
        controls.connected.remove(&companion);
        return Err(error);
    }
    let completed = app
        .0
        .repository
        .complete_receipt(Receipt {
            key: key.clone(),
            target,
            request: body,
            outcome: Outcome::Accepted,
            response: response.clone(),
        })
        .await;
    app.0.controls.lock().await.inflight.remove(&key);
    completed?;
    Ok((Outcome::Accepted, response))
}

/// The companions whose connected model configuration no longer matches the
/// configuration just committed. Their running turns are stopped so they do
/// not continue under a changed model, credential or grant.
pub(super) fn reconfigured(
    app: &HostApplication,
    controls: &Controls,
    configuration: &Configuration,
) -> Vec<String> {
    controls
        .connected
        .iter()
        .filter_map(|(companion, connected)| {
            let current = configuration.companions.get(companion)?;
            let current_signature = (|| {
                let assignment = current.model_assignments.companion.as_option()?;
                let connection = rules::connection(configuration, &assignment.connection_id)?;
                let policy = app.0.providers.resolve(&connection).ok()?.policy().clone();
                let nonce = controls
                    .secrets
                    .get(&connection.id)
                    .map(|secret| secret.nonce.clone());
                Some(signature(
                    configuration,
                    companion,
                    current,
                    &connection,
                    &policy,
                    nonce.as_deref(),
                ))
            })();
            (current_signature.as_deref() != Some(connected.as_str())).then(|| companion.clone())
        })
        .collect()
}

pub(crate) async fn cancel(app: HostApplication, companion: String) -> crate::ports::Result<()> {
    validate_id(&companion)?;
    app.companion(&companion).await?;
    // A host without a kernel has no parent turn to stop; its other
    // execution registrations are still cancelled below.
    let runtime = app.0.rig.get().cloned();
    let controls = app.0.controls.lock().await;
    for run in controls
        .running
        .values()
        .filter(|run| run.companion == companion)
    {
        run.cancel.send_replace(true);
    }
    drop(controls);
    // Acceptance only: the turn's end is reported by its TurnEvent.
    match runtime {
        Some(runtime) => runtime.stop(companion, None),
        None => Ok(()),
    }
}

pub(crate) async fn history(
    app: &HostApplication,
    companion: String,
    cursor: Option<String>,
    limit: usize,
) -> crate::ports::Result<ene_rpc::History> {
    app.companion(&companion).await?;
    if !(1..=200).contains(&limit) {
        return Err(Error::invalid());
    }
    // A malformed cursor is refused by the kernel's history reader.
    app.rig()?.history(companion, cursor, limit).await
}

pub(crate) async fn message(
    app: &HostApplication,
    companion: String,
    id: String,
) -> crate::ports::Result<Message> {
    validate_id(&companion)?;
    validate_id(&id)?;
    app.companion(&companion).await?;
    app.rig()?.message(companion, id).await
}

impl HostApplication {
    #[cfg(test)]
    pub(crate) fn set_test_model(&self, model: Arc<TestModel>) {
        let _ = self.0.test_model.set(model);
    }

    /// Test-only: leave a chat receipt pending as a crash between the kernel's
    /// log write and the receipt completion would.
    #[cfg(test)]
    pub(crate) async fn reserve_test_chat_receipt(
        &self,
        companion: &str,
        key: &str,
        text: &str,
        request_id: &str,
    ) -> crate::ports::Result<()> {
        self.0
            .repository
            .reserve(Receipt {
                key: key.into(),
                target: format!("/v1/companions/{companion}/messages"),
                request: json!({
                    "text": text,
                    "resources": [],
                    "question_id": null,
                    "decision": null,
                }),
                outcome: Outcome::Pending,
                response: json!({"requestId": request_id}),
            })
            .await
    }

    /// Attaches the kernel and consumes what it did. Called once, before the
    /// host accepts messages.
    pub(crate) fn attach_rig(&self, runtime: Runtime, mut events: UnboundedReceiver<Event>) {
        if self.0.rig.set(Arc::new(runtime)).is_err() {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                app.kernel_event(event).await;
            }
        });
    }

    pub(crate) fn rig(&self) -> crate::ports::Result<Arc<Runtime>> {
        self.0.rig.get().cloned().ok_or(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("execution_unavailable"),
        })
    }

    async fn kernel_event(&self, event: Event) {
        match event {
            Event::TurnStarted {
                companion,
                stream_id,
                request_id,
                messages,
            } => {
                self.character_event(&companion, ene_plugin::CharacterEvent::ConversationStarted);

                let mut boundary = self.0.boundary.lock().await;
                for message in messages {
                    publish(
                        &mut boundary,
                        self,
                        Some(&companion),
                        ene_rpc::notification::Event::MessageCreated(Box::new(message)),
                    );
                }
                publish(
                    &mut boundary,
                    self,
                    Some(&companion),
                    ene_rpc::notification::Event::TurnEvent(Box::new(ene_rpc::TurnEvent {
                        companion_id: companion.clone(),
                        stream_id,
                        state: "streaming".into(),
                        request_id,
                        ..Default::default()
                    })),
                );
            }
            Event::Delta {
                companion,
                stream_id,
                offset,
                delta,
            } => {
                let mut boundary = self.0.boundary.lock().await;
                publish(
                    &mut boundary,
                    self,
                    Some(&companion),
                    ene_rpc::notification::Event::MessageDelta(Box::new(ene_rpc::MessageDelta {
                        stream_id,
                        offset,
                        delta,
                        ..Default::default()
                    })),
                );
            }
            Event::TurnFinished {
                companion,
                stream_id,
                request_id,
                state,
                message_id,
                messages,
                failure,
            } => {
                let mut boundary = self.0.boundary.lock().await;
                for message in messages {
                    publish(
                        &mut boundary,
                        self,
                        Some(&companion),
                        ene_rpc::notification::Event::MessageCreated(Box::new(message)),
                    );
                }
                publish(
                    &mut boundary,
                    self,
                    Some(&companion),
                    ene_rpc::notification::Event::TurnEvent(Box::new(ene_rpc::TurnEvent {
                        companion_id: companion.clone(),
                        stream_id,
                        state: state.into(),
                        request_id: request_id.clone(),
                        message_id,
                        failure: failure.into(),
                        ..Default::default()
                    })),
                );
                self.notify_mind_work();
                if let Some(request_id) = request_id.as_deref() {
                    self.clear_active_recall(&companion, request_id);
                }
            }
            Event::StorageFailed { reason } => {
                let mut controls = self.0.controls.lock().await;
                let mut boundary = self.0.boundary.lock().await;
                controls.fail_storage_with(self, &mut boundary, Some(reason));
                drop(boundary);
                // A turn that continues cannot be saved; stop the running
                // ones and let their stopped ends be shown, not saved.
                let companions = controls
                    .config
                    .as_ref()
                    .map(|configuration| {
                        configuration.companions.keys().cloned().collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                drop(controls);
                if let Ok(runtime) = self.rig() {
                    for companion in companions {
                        let _ = runtime.stop(companion, Some("storage_error".into()));
                    }
                }
            }
            Event::Recalled {
                companion,
                text,
                why,
            } => {
                let mut boundary = self.0.boundary.lock().await;
                publish(
                    &mut boundary,
                    self,
                    Some(&companion),
                    ene_rpc::notification::Event::RecalledInput(Box::new(ene_rpc::RecalledInput {
                        companion_id: companion.clone(),
                        text,
                        reason: why,
                        ..Default::default()
                    })),
                );
            }
            Event::Fatal { reason } => {
                let mut controls = self.0.controls.lock().await;
                let mut boundary = self.0.boundary.lock().await;
                controls.fail_execution(self, &mut boundary, reason);
            }
        }
    }
}
