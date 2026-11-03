//! Host invocation path: one binding, one operation, validated events and services.
use super::sessions::{ActiveInvocation, Session, SessionState};
use super::{Manager, conflict, failure, invocation_failure, storage_failure, unsupported};
use crate::infrastructure::workspace::Workspace;
use crate::{
    domain::{Error, ErrorKind, harness::Session as HarnessSession},
    ports::{Result, inference::InferenceRequest, plugin::Mutations},
};
use ene_plugin::{Event, Failure, HarnessEvent, Operation, ServiceRequest, Target};
use ene_plugin_runtime::{
    instance::{Instance, Services},
    limits::{MAX_EVENTS, MAX_MESSAGE_BYTES, MAX_SESSIONS, MAX_STREAM_BYTES},
};
use ene_rpc::{PluginBinding, PluginInvocation, PluginResult};
use futures_util::{TryFutureExt, future::BoxFuture};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

impl Manager {
    pub async fn invoke(
        &self,
        input: PluginInvocation,
        mutations: Arc<dyn Mutations>,
    ) -> Result<PluginResult> {
        // The binding read and the session it produces must not straddle a rebind.
        let binding_lock = self.binding.lock().await;
        let binding: PluginBinding = serde_json::from_value(
            self.store
                .plugin_get("bindings".into(), input.binding_id)
                .await
                .map_err(storage_failure)?,
        )
        .map_err(failure)?;
        let entry = self.ready(&binding.plugin_id, Some(&binding.revision))?;
        let operation: Operation =
            serde_json::from_str(&input.operation.json).map_err(|_| Error::invalid())?;
        let endpoint = entry.package.entry(&binding.entrypoint).map_err(failure)?;
        if !endpoint.provides.contains(&operation.extension())
            || matches!(
                operation,
                Operation::Generate { .. } | Operation::ListModels { .. }
            )
        {
            return Err(unsupported());
        }
        if endpoint.target == Target::Acp {
            return self
                .invoke_acp(binding, entry, operation, binding_lock)
                .await;
        }
        if let Operation::Character { target, .. } = &operation
            && (target.device_id != binding.device_id
                || target.character_id != binding.companion_id
                || !ene_plugin_runtime::package::identifier(&target.device_id))
        {
            return Err(Error::new(ErrorKind::PermissionRequired));
        }
        if let Operation::Character {
            event: ene_plugin::CharacterEvent::Custom { name, .. },
            ..
        } = &operation
            && (!name.starts_with(&format!("{}:", binding.plugin_id)) || name.len() > 160)
        {
            return Err(Error::invalid());
        }
        let harness = matches!(operation, Operation::Harness { .. });
        // Cancel reaches the active invocation before waiting for its worker lock.
        let cancelling = matches!(
            &operation,
            Operation::Harness {
                input: ene_plugin::HarnessInput::Cancel
            }
        );
        if cancelling
            && let Some(session) = self.sessions.lock().unwrap().get(&binding.id)
            && let Some(token) = session.active.lock().unwrap().as_ref()
        {
            token.cancel();
        }
        let permit = if cancelling {
            None
        } else {
            Some(
                self.invocation_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::unavailable())?,
            )
        };
        let key = binding.id.clone();
        let existing = self.sessions.lock().unwrap().get(&key).cloned();
        if let Some(stale) = existing.filter(|session| !session.matches(&binding)) {
            // A session that belongs to another binding is only replaced once its stop is durable.
            self.stop_session(&key, &stale).await?;
            return Err(conflict());
        }
        let worker = {
            let mut sessions = self.sessions.lock().unwrap();
            let existing = sessions.get(&key).cloned();
            match existing {
                Some(session) => session,
                None => {
                    if sessions.len() >= MAX_SESSIONS {
                        return Err(Error::unavailable());
                    }
                    let session = Arc::new(SessionState::new(
                        &binding,
                        entry.cancellation.child_token(),
                    ));
                    sessions.insert(key.clone(), session.clone());
                    session
                }
            }
        };
        // A cancel keeps the binding lock until its terminal record is durable.
        let _cancel_binding = cancelling.then_some(binding_lock);
        let lifetime = &worker.lifetime;
        let guard = worker.worker.lock().await;
        let mut session = Session {
            retain: guard.is_some() || lifetime.is_cancelled(),
            guard,
            registration: (self.clone(), key.clone(), worker.clone()),
            _permit: permit,
        };
        if lifetime.is_cancelled() && !cancelling {
            return Err(Error::unavailable());
        }
        if !cancelling
            && !self
                .sessions
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &worker))
        {
            return Err(conflict());
        }
        if cancelling {
            let journal = self
                .harness_cancel(&binding.id)
                .await
                .inspect_err(|error| {
                    if error.kind == ErrorKind::StorageFailure {
                        session.take();
                        lifetime.cancel();
                        session.retain = true;
                    }
                })?;
            session.retain = false;
            session.take();
            return Ok(PluginResult {
                generation: entry.generation,
                events: harness_terminal(&journal),
                result: ene_rpc::JsonDocument::from(super::harness_result(&journal)).into(),
                ..Default::default()
            });
        }
        let cancel = lifetime.child_token();
        let _invocation = ActiveInvocation::new(worker.clone(), cancel.clone());
        // A suspended permission request lives in the guest instance that
        // created it. A fresh instance must not resume a turn whose state is gone.
        if matches!(
            &operation,
            Operation::Harness {
                input: ene_plugin::HarnessInput::Respond { .. }
            }
        ) && session.is_none()
        {
            return Err(conflict());
        }
        let journal = if harness {
            match &operation {
                Operation::Harness { input } => Some(self.harness_begin(&binding, input).await?),
                _ => unreachable!("harness operations carry an input"),
            }
        } else {
            None
        };
        session.retain = false;
        let stopping = matches!(
            operation,
            Operation::Character {
                event: ene_plugin::CharacterEvent::Deactivated,
                ..
            }
        );
        let mut failure_reason = "startup_failed";
        // An event the host rejected stops the guest through `cancel`; the
        // terminal record must still be a failure, not a client cancellation.
        let rejected = Arc::new(AtomicBool::new(false));
        let outcome = async {
            if session.is_some() && matches!(&operation, Operation::Speech { .. }) {
                session
                    .as_mut()
                    .unwrap()
                    .initialize(&cancel)
                    .await
                    .map_err(failure)?;
            }
            if session.is_none() {
                let verified = entry.package.clone();
                *session = Some(
                    Instance::load(
                        verified,
                        binding.entrypoint.clone(),
                        Target::Host,
                        self.generations.fetch_add(1, Ordering::Relaxed),
                        &cancel,
                    )
                    .await
                    .map_err(failure)?,
                );
            }
            failure_reason = "invocation_failed";
            let services = Arc::new(LocalServices {
                manager: self.clone(),
                binding: binding.clone(),
                cancel: cancel.clone(),
                mutations,
            });
            let (tx, mut rx) = mpsc::channel(16);
            let validation = services.clone();
            let worker = session.as_mut().unwrap();
            let generation = worker.generation();
            let work = worker
                .invoke(operation, services, tx, &cancel)
                .map_err(invocation_failure);
            let (events, result, journal) = if let Some(journal) = journal {
                let manager = self.clone();
                let binding_id = binding.id.clone();
                let stop = cancel.clone();
                let rejected = rejected.clone();
                let applier = async move {
                    let mut journal = journal;
                    // A whole ACP turn may stream many chunks. The journal is
                    // bounded on its own; the invocation result keeps the
                    // newest events so a long turn does not return megabytes.
                    let mut events = std::collections::VecDeque::new();
                    let mut bytes = 0;
                    while let Some(event) = rx.recv().await {
                        let applied = async {
                            bytes += serde_json::to_vec(&event).map_err(failure)?.len();
                            if bytes > MAX_STREAM_BYTES {
                                return Err(Error::invalid());
                            }
                            let Event::Harness {
                                event: harness_event,
                            } = &event
                            else {
                                return Err(Error::invalid());
                            };
                            manager
                                .harness_apply(&binding_id, &mut journal, harness_event)
                                .await?;
                            events.push_back(event);
                            if events.len() > MAX_EVENTS {
                                events.pop_front();
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = applied {
                            // Stops a guest that keeps running after a rejected event.
                            rejected.store(true, Ordering::Relaxed);
                            stop.cancel();
                            return Err(error);
                        }
                    }
                    Ok::<_, Error>((journal, events.into_iter().collect()))
                };
                let (worked, applied) = tokio::join!(work, applier);
                let (journal, events) = applied?;
                worked?;
                let result = super::harness_result(&journal);
                (events, result, Some(journal))
            } else {
                let collect = async move {
                    let mut events = Vec::new();
                    let mut bytes = 0;
                    while let Some(event) = rx.recv().await {
                        bytes += serde_json::to_vec(&event).map_err(failure)?.len();
                        if events.len() >= MAX_EVENTS || bytes > MAX_MESSAGE_BYTES {
                            return Err(Error::invalid());
                        }
                        events.push(event);
                    }
                    Ok::<_, Error>(events)
                };
                let (events, result) = tokio::try_join!(collect, work)?;
                (events, result, None::<HarnessSession>)
            };
            validation.check().await.map_err(|failure| {
                if failure.code == "state_unavailable" {
                    Error::new(ErrorKind::StorageFailure)
                } else {
                    Error::new(ErrorKind::PermissionRequired)
                }
            })?;
            if self
                .ready(&binding.plugin_id, Some(&binding.revision))?
                .generation
                != entry.generation
            {
                return Err(conflict());
            }
            Ok((generation, events, result, journal))
        }
        .await;
        let rejected = rejected.load(Ordering::Relaxed);
        let terminal = matches!(
            &outcome,
            Ok((_, _, _, Some(journal))) if journal.terminal()
        );
        let stalled = matches!(
            &outcome,
            Ok((_, _, _, Some(journal))) if !journal.terminal() && journal.pending.is_none()
        );
        if stopping || terminal || stalled || outcome.is_err() || cancel.is_cancelled() {
            session.take();
        }
        if stalled {
            self.harness_finish(&binding.id, &cancel, lifetime, "harness_stalled", false)
                .await?;
            return Err(failure("harness_stalled"));
        }
        if outcome.is_err() || cancel.is_cancelled() {
            if harness {
                // One Host-owned path classifies and persists every unsuccessful turn.
                self.harness_finish(&binding.id, &cancel, lifetime, failure_reason, rejected)
                    .await
                    .inspect_err(|_| {
                        lifetime.cancel();
                        session.retain = true;
                    })?;
            }
            return Err(outcome.err().unwrap_or_else(conflict));
        }
        let (instance_generation, events, result, _) = outcome?;
        session.retain = !stopping && !terminal;
        Ok(PluginResult {
            generation: instance_generation,
            events: events
                .into_iter()
                .map(|event| {
                    serde_json::to_value(event)
                        .map(ene_rpc::JsonDocument::from)
                        .map_err(failure)
                })
                .collect::<Result<_>>()?,
            result: ene_rpc::JsonDocument::from(result).into(),
            ..Default::default()
        })
    }
}

fn harness_terminal(journal: &HarnessSession) -> Vec<ene_rpc::JsonDocument> {
    journal
        .events
        .iter()
        .rev()
        .find(|event| matches!(event, HarnessEvent::Stop { .. }))
        .map(|event| {
            vec![ene_rpc::JsonDocument::from(
                serde_json::to_value(Event::Harness {
                    event: event.clone(),
                })
                .expect("serializable harness event"),
            )]
        })
        .unwrap_or_default()
}

pub(super) struct LocalServices {
    pub(super) manager: Manager,
    pub(super) binding: PluginBinding,
    pub(super) cancel: CancellationToken,
    pub(super) mutations: Arc<dyn Mutations>,
}
impl Services for LocalServices {
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async {
            if self.cancel.is_cancelled()
                || self
                    .manager
                    .ready(&self.binding.plugin_id, Some(&self.binding.revision))
                    .is_err()
            {
                return Err(Failure::new("revoked"));
            }
            let current = self
                .manager
                .store
                .plugin_get("bindings".into(), self.binding.id.clone())
                .await
                .map_err(|_| Failure::new("state_unavailable"))?;
            if current != serde_json::to_value(&self.binding).unwrap_or_default() {
                return Err(Failure::new("revoked"));
            }
            Ok(())
        })
    }
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async move {
            self.check().await?;
            if !self
                .binding
                .services
                .contains(&super::name(request.service()))
            {
                return Err(Failure::new("permission_required"));
            }
            let namespace = format!(
                "state:{}:{}:{}",
                self.binding.plugin_id, self.binding.entrypoint, self.binding.companion_id
            );
            match request {
                ServiceRequest::Log { message } if message.len() <= 4096 => {
                    eprintln!("ene-host plugin [{}]: {message:?}", self.binding.plugin_id);
                    Ok(Value::Null)
                }
                ServiceRequest::Configuration => {
                    serde_json::from_str(&self.binding.configuration.json)
                        .map_err(|_| Failure::new("invalid_configuration"))
                }
                ServiceRequest::StateRead { key } if valid_key(&key) => self
                    .manager
                    .store
                    .plugin_get(namespace, key)
                    .await
                    .map_err(|_| Failure::new("state_unavailable")),
                ServiceRequest::Tool { name, arguments } => {
                    let root = self
                        .binding
                        .workspace_root
                        .as_deref()
                        .filter(|root| !root.is_empty())
                        .ok_or_else(|| Failure::new("permission_required"))?;
                    let workspace = Workspace::open(root).map_err(workspace_failure)?;
                    match name.as_str() {
                        "fs.read_text_file" => {
                            let input: ReadFileInput = serde_json::from_value(arguments)
                                .map_err(|_| Failure::new("invalid_request"))?;
                            let read = workspace
                                .read(&input.path, input.line, input.limit)
                                .map_err(workspace_failure)?;
                            Ok(json!({"content": read.content, "truncated": read.truncated}))
                        }
                        "fs.write_text_file" => {
                            let input: WriteFileInput = serde_json::from_value(arguments)
                                .map_err(|_| Failure::new("invalid_request"))?;
                            workspace
                                .write(&input.path, &input.content)
                                .map_err(workspace_failure)?;
                            Ok(json!({"ok": true}))
                        }
                        _ => Err(Failure::new("unsupported_tool")),
                    }
                }
                ServiceRequest::Models { request } => {
                    let inference = self
                        .manager
                        .inference()
                        .ok_or_else(|| Failure::new("unavailable"))?;
                    let messages = request
                        .messages
                        .iter()
                        .map(|message| match message.role.as_str() {
                            "user" => Some((true, message.text.clone())),
                            "assistant" => Some((false, message.text.clone())),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| Failure::new("invalid_request"))?;
                    let text = inference
                        .complete(
                            InferenceRequest {
                                companion_id: self.binding.companion_id.clone(),
                                preamble: request.preamble,
                                messages,
                                max_tokens: request.max_tokens,
                            },
                            self.cancel.clone(),
                        )
                        .await
                        .map_err(|error| Failure::new(failure_code(error.kind)))?;
                    Ok(json!({"text": text}))
                }
                ServiceRequest::StateWrite { key, value }
                    if valid_key(&key)
                        && serde_json::to_vec(&value).is_ok_and(|v| v.len() <= 65536) =>
                {
                    let store = self.manager.store.clone();
                    self.mutations
                        .submit(Box::pin(async move {
                            store
                                .plugin_set(namespace, key, value)
                                .await
                                .map_err(crate::infrastructure::sqlite::core_error)
                        }))
                        .await
                        .map_err(|_| Failure::new("state_unavailable"))?;
                    Ok(Value::Null)
                }
                _ => Err(Failure::new("unsupported_service")),
            }
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileInput {
    path: String,
    content: String,
}
fn workspace_failure(error: Error) -> Failure {
    let code = match error.kind {
        ErrorKind::PermissionRequired => "permission_required",
        ErrorKind::NotFound => "not_found",
        ErrorKind::ResourceLimit => "resource_limit",
        ErrorKind::InvalidRequest => "invalid_request",
        _ => "workspace_io",
    };
    Failure::new(code)
}
fn failure_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::PermissionRequired => "permission_required",
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::ResourceLimit => "output_limit",
        ErrorKind::StateConflict => "model_unset",
        ErrorKind::InvalidRequest => "invalid_request",
        _ => "upstream_error",
    }
}
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control)
}
