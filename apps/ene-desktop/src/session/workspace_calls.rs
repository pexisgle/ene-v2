//! Bounds workspace calls to the stream and the run that authorized them.
use super::{Session, commands::OperationError, device_commands::OperationOutput};
use crate::{host::Host, work_tools::Workspace};
use ene_rpc as w;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::task::{Id, JoinError, JoinSet};
use tokio_util::sync::{CancellationToken, DropGuard};

pub(super) struct WorkspaceCalls {
    // Cancel blocking workers before dropping their async result handles.
    active: HashMap<Id, (String, DropGuard)>,
    tasks: JoinSet<w::DeviceOperationResult>,
}

impl WorkspaceCalls {
    pub(super) fn new() -> Self {
        Self {
            active: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    pub(super) fn admit(&self, operation: &w::DeviceOperation) -> Result<String, OperationError> {
        if operation.work_id.as_deref().is_none_or(str::is_empty) {
            return Err(OperationError::WorkMissingSession);
        }
        if self.tasks.len() >= 8 {
            return Err(OperationError::WorkspaceBusy);
        }
        operation
            .run_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or(OperationError::WorkMissingSession)
    }

    pub(super) fn spawn(
        &mut self,
        host: &Host,
        mut operation: w::DeviceOperation,
        run_id: String,
        workspace: Arc<Workspace>,
    ) {
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let host = host.clone();
        let task = self.tasks.spawn(async move {
            let outcome = async {
                // Device operations and run notifications arrive on separate
                // streams. A delayed operation can follow its run's terminal event.
                require_dispatched(&host, &operation).await?;
                let input = std::mem::take(&mut operation.input);
                Box::pin(workspace.execute(&operation.capability, input, worker_stop))
                    .await
                    .map(OperationOutput::External)
                    .map_err(OperationError::from)
            }
            .await;
            Session::operation_result(&operation.client_id, &operation, outcome)
        });
        self.active.insert(task.id(), (run_id, stop.drop_guard()));
    }

    pub(super) async fn finished(&mut self) -> Option<Result<w::DeviceOperationResult, JoinError>> {
        let result = self.tasks.join_next_with_id().await?;
        let id = result.as_ref().map_or_else(JoinError::id, |(id, _)| *id);
        self.active.remove(&id);
        Some(result.map(|(_, result)| result))
    }

    pub(super) async fn report(host: &Host, result: Result<w::DeviceOperationResult, JoinError>) {
        match result {
            Ok(result) => Session::report_operation(host, result).await,
            Err(error) => tracing::warn!("workspace operation did not finish: {error}"),
        }
    }

    pub(super) fn on_notification(&self, notification: &w::Notification) {
        if let Some(w::notification::Event::WorkEvent(event)) = &notification.event
            && let Some(w::work_event::Detail::RunFinished(finished)) = &event.detail
            && matches!(
                finished.state.as_known(),
                Some(w::RunStatus::Completed | w::RunStatus::Failed | w::RunStatus::Cancelled)
            )
        {
            for (run_id, stop) in self.active.values() {
                if run_id == &event.run_id {
                    stop.token().cancel();
                }
            }
        }
    }
}

async fn require_dispatched(
    host: &Host,
    operation: &w::DeviceOperation,
) -> Result<(), OperationError> {
    let state = tokio::time::timeout(
        Duration::from_secs(2),
        host.client.get_operation(w::Id {
            id: operation.operation_id.clone(),
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| OperationError::RunVerifyTimeout)?
    .map_err(|_| OperationError::RunVerifyFailed)?;
    let state = state.view();
    if state.status == w::DeviceOperationStatus::Dispatched
        && state.operation_id == operation.operation_id
        && state.client_id == operation.client_id
    {
        Ok(())
    } else {
        Err(OperationError::WorkOperationInactive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_terminal_run_notification_cancels_only_its_own_workspace_calls() {
        let mut calls = WorkspaceCalls::new();
        let first = keep_pending(&mut calls, "first");
        let other = keep_pending(&mut calls, "other");
        calls.on_notification(&w::Notification {
            event: Some(w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
                run_id: "first".into(),
                detail: Some(w::work_event::Detail::RunFinished(Box::new(
                    w::WorkRunFinished {
                        state: w::RunStatus::Cancelled.into(),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            }))),
            ..Default::default()
        });
        assert!(first.is_cancelled());
        assert!(!other.is_cancelled());
        drop(calls);
        assert!(other.is_cancelled());
    }

    fn keep_pending(calls: &mut WorkspaceCalls, run: &str) -> CancellationToken {
        let stop = CancellationToken::new();
        let task = calls.tasks.spawn(std::future::pending());
        calls
            .active
            .insert(task.id(), (run.into(), stop.clone().drop_guard()));
        stop
    }
}
