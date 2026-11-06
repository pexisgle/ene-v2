//! External ACP agent backend for harness bindings.
use super::sessions::AcpSlot;
use super::*;
use crate::domain::harness::Session as HarnessSession;
use crate::infrastructure::{
    acp::{AcpConfig, AcpSession, MAX_UPDATES, TurnState},
    workspace::Workspace,
};
use ene_plugin::{Event, HarnessEvent, HarnessInput, Operation};
use ene_plugin_runtime::limits::MAX_EVENTS;
use ene_rpc::PluginBinding;
use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use tokio::sync::{MutexGuard, mpsc};
use tokio_util::sync::CancellationToken;

impl Manager {
    pub(super) async fn invoke_acp(
        &self,
        binding: PluginBinding,
        entry: Entry,
        operation: Operation,
        binding_lock: MutexGuard<'_, ()>,
    ) -> Result<Completed> {
        let input = match &operation {
            Operation::Harness { input } => input.clone(),
            _ => return Err(unsupported()),
        };
        // ACP agents run as processes with their own model/tool access; host
        // services are not exposed to them.
        if !binding.services.is_empty() {
            return Err(unsupported());
        }
        let cancelling = matches!(input, HarnessInput::Cancel);
        let _permit = if cancelling {
            None
        } else {
            Some(
                self.invocation_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::unavailable())?,
            )
        };
        let slot = self.acp_slot(&binding);
        if cancelling {
            // The turn invocation drops the binding lock only after it holds
            // the session lock, so this point is past session creation.
            slot.cancel.cancel();
            let mut session = slot.session.lock().await;
            if let Some(session) = session.take() {
                session.kill().await;
            }
            drop(session);
            drop(binding_lock);
            let journal = self.harness_cancel(&binding.id).await?;
            self.acp_remove(&binding.id);
            return Ok(completed(entry.generation, &journal, Vec::new()));
        }
        let mut session = slot.session.lock().await;
        // A response belongs to the process that asked; never resume a turn
        // whose ACP session is gone.
        if matches!(input, HarnessInput::Respond { .. }) && session.is_none() {
            self.acp_remove(&binding.id);
            return Err(conflict());
        }
        let mut journal = match self.harness_begin(&binding, &input).await {
            Ok(journal) => journal,
            Err(error) => {
                if session.is_none() {
                    self.acp_remove(&binding.id);
                }
                return Err(error);
            }
        };
        drop(binding_lock);
        let mut events = VecDeque::new();
        let outcome = async {
            if session.is_none() {
                let root = binding
                    .workspace_root
                    .as_deref()
                    .filter(|root| !root.is_empty())
                    .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
                let workspace = Workspace::open(root)?;
                let command = entry
                    .package
                    .entry(&binding.entrypoint)
                    .map_err(failure)?
                    .command
                    .clone();
                let terminal = binding
                    .terminal
                    .as_option()
                    .filter(|policy| policy.enabled)
                    .map(|policy| crate::infrastructure::acp::TerminalLimits {
                        allowed: policy.allowed_commands.clone(),
                        allowed_env: policy.allowed_env.clone(),
                        max_seconds: policy.max_seconds as u64,
                        max_output: policy.max_output_bytes as usize,
                    });
                *session = Some(
                    AcpSession::start(AcpConfig {
                        command,
                        workspace,
                        cwd: PathBuf::from(root),
                        terminal,
                    })
                    .await?,
                );
            }
            let agent = session.as_mut().ok_or_else(Error::unavailable)?;
            // Each confirmed update is durable before the turn's next event,
            // so a long turn checkpoints its progress.
            let turn = {
                let (updates, mut pending) = mpsc::channel(MAX_UPDATES);
                let apply = async {
                    while let Some(update) = pending.recv().await {
                        let event = HarnessEvent::Update { update };
                        self.harness_apply(&binding.id, &mut journal, &event)
                            .await?;
                        remember(&mut events, event);
                    }
                    Ok::<_, Error>(())
                };
                let mut apply = std::pin::pin!(apply);
                let turn = async {
                    match &input {
                        HarnessInput::Start { prompt } => agent.prompt(prompt, updates).await,
                        HarnessInput::Respond {
                            request_id,
                            outcome,
                        } => agent.respond(request_id, outcome, updates).await,
                        HarnessInput::Cancel => unreachable!("cancel is handled above"),
                    }
                };
                let mut turn = std::pin::pin!(turn);
                let turn = tokio::select! { biased;
                    _ = slot.cancel.cancelled() => return Err(Error::new(ErrorKind::Unavailable)),
                    applied = apply.as_mut() => { applied?; turn.await }
                    turn = &mut turn => turn,
                }?;
                apply.await?;
                turn
            };
            let event = match turn.state {
                TurnState::Waiting(waiting) => HarnessEvent::RequestPermission {
                    request_id: waiting.request_id,
                    tool_call: waiting.tool_call,
                    options: waiting.options,
                },
                TurnState::Stopped(stop_reason) => HarnessEvent::Stop { stop_reason },
            };
            self.harness_apply(&binding.id, &mut journal, &event)
                .await?;
            remember(&mut events, event);
            Ok::<_, Error>(())
        }
        .await;
        match outcome {
            Ok(()) if journal.terminal() => {
                if let Some(agent) = session.take() {
                    agent.kill().await;
                }
                self.acp_remove(&binding.id);
                Ok(completed(
                    entry.generation,
                    &journal,
                    events.into_iter().collect(),
                ))
            }
            Ok(()) => Ok(completed(
                entry.generation,
                &journal,
                events.into_iter().collect(),
            )),
            Err(error) => {
                if let Some(agent) = session.take() {
                    agent.kill().await;
                }
                self.acp_remove(&binding.id);
                if !slot.cancel.is_cancelled() {
                    self.harness_stop(&binding.id, "acp_failed").await?;
                }
                Err(error)
            }
        }
    }

    pub(super) fn acp_slot(&self, binding: &PluginBinding) -> Arc<AcpSlot> {
        let mut slots = self.acp.lock().unwrap();
        slots
            .entry(binding.id.clone())
            .or_insert_with(|| {
                Arc::new(AcpSlot {
                    plugin_id: binding.plugin_id.clone(),
                    cancel: CancellationToken::new(),
                    session: tokio::sync::Mutex::new(None),
                })
            })
            .clone()
    }
    pub(super) fn acp_remove(&self, binding_id: &str) {
        self.acp.lock().unwrap().remove(binding_id);
    }
    /// Kills one agent process and records the task as stopped.
    pub(super) async fn stop_acp(&self, binding_id: &str, reason: &str) -> Result<()> {
        let slot = self.acp.lock().unwrap().remove(binding_id);
        let Some(slot) = slot else {
            return Ok(());
        };
        slot.cancel.cancel();
        let mut session = slot.session.lock().await;
        if let Some(agent) = session.take() {
            agent.kill().await;
        }
        drop(session);
        self.harness_stop(binding_id, reason).await
    }
    /// Stops every ACP session, optionally only those of one package.
    pub(super) async fn stop_acp_sessions(&self, package: Option<&str>) -> Result<()> {
        let slots: Vec<String> = self
            .acp
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, slot)| package.is_none_or(|id| id == slot.plugin_id))
            .map(|(id, _)| id.clone())
            .collect();
        let mut result = Ok(());
        for id in slots {
            let stopped = self.stop_acp(&id, "plugin_stopped").await;
            if result.is_ok() {
                result = stopped;
            }
        }
        result
    }
}

fn remember(events: &mut VecDeque<Event>, event: HarnessEvent) {
    events.push_back(Event::Harness { event });
    if events.len() > MAX_EVENTS {
        events.pop_front();
    }
}

fn completed(generation: u64, journal: &HarnessSession, events: Vec<Event>) -> Completed {
    Completed {
        generation,
        events,
        answer: Answer::Harness(Box::new(journal.clone())),
    }
}
