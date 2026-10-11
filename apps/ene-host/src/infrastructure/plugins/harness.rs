//! The Host owns task identity, pending operations, consultation and final reports.
use super::*;
use ene_plugin::{HarnessEvent, HarnessInput};
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Journal {
    task_id: String,
    status: String,
    pending: Option<String>,
    question: bool,
    events: Vec<Value>,
    report: Option<String>,
}
impl Manager {
    pub(super) async fn harness_begin(
        &self,
        binding: &PluginBinding,
        input: &HarnessInput,
    ) -> Result<Journal> {
        let value = self
            .store
            .plugin_get("harness".into(), binding.id.clone())
            .await
            .map_err(failure)?;
        let old = if value.is_null() {
            None
        } else {
            Some(serde_json::from_value::<Journal>(value).map_err(failure)?)
        };
        match input {
            HarnessInput::Start { .. } => {
                if old.as_ref().is_some_and(|j| {
                    !matches!(j.status.as_str(), "completed" | "failed" | "cancelled")
                }) {
                    return Err(conflict());
                }
                Ok(Journal {
                    task_id: uuid::Uuid::new_v4().to_string(),
                    status: "running".into(),
                    pending: None,
                    question: false,
                    events: Vec::new(),
                    report: None,
                })
            }
            HarnessInput::Cancel => old.ok_or_else(conflict),
            HarnessInput::Answer { question_id, .. } => old
                .filter(|j| j.question && j.pending.as_ref() == Some(question_id))
                .ok_or_else(conflict),
            HarnessInput::Result { operation_id, .. } => old
                .filter(|j| !j.question && j.pending.as_ref() == Some(operation_id))
                .ok_or_else(conflict),
        }
    }
    pub(super) async fn harness_cancel(
        &self,
        binding: &PluginBinding,
        journal: Journal,
        events: &[Value],
    ) -> Result<String> {
        if journal.status == "cancelled" {
            return Ok(journal.task_id);
        }
        self.harness_commit(binding, journal, events).await
    }
    pub(super) async fn harness_finish(
        &self,
        binding: &PluginBinding,
        journal: Journal,
        cancel: &CancellationToken,
        lifetime: &CancellationToken,
        failure_reason: &str,
    ) -> Result<String> {
        let event = if lifetime.is_cancelled() {
            HarnessEvent::Failed {
                reason: "plugin_stopped".into(),
            }
        } else if cancel.is_cancelled() {
            HarnessEvent::Cancelled
        } else {
            HarnessEvent::Failed {
                reason: failure_reason.into(),
            }
        };
        self.harness_commit(
            binding,
            journal,
            &[serde_json::to_value(Event::Harness { event }).map_err(failure)?],
        )
        .await
    }
    pub(super) async fn harness_commit(
        &self,
        binding: &PluginBinding,
        mut journal: Journal,
        events: &[Value],
    ) -> Result<String> {
        journal.pending = None;
        journal.question = false;
        for value in events {
            let Event::Harness { event } =
                serde_json::from_value::<Event>(value.clone()).map_err(failure)?
            else {
                return Err(Error::invalid());
            };
            if matches!(
                journal.status.as_str(),
                "completed" | "failed" | "cancelled"
            ) {
                return Err(conflict());
            }
            match event {
                HarnessEvent::ModelRequest { operation_id, .. }
                | HarnessEvent::ToolRequest { operation_id, .. } => {
                    if journal.pending.replace(operation_id).is_some() {
                        return Err(conflict());
                    }
                    journal.status = "waiting_operation".into();
                }
                HarnessEvent::Question { question_id, .. } => {
                    if journal.pending.replace(question_id).is_some() {
                        return Err(conflict());
                    }
                    journal.question = true;
                    journal.status = "waiting_answer".into();
                }
                HarnessEvent::Completed { report } => {
                    journal.status = "completed".into();
                    journal.report = Some(report);
                }
                HarnessEvent::Failed { reason } => {
                    journal.status = "failed".into();
                    journal.report = Some(reason);
                }
                HarnessEvent::Cancelled => journal.status = "cancelled".into(),
                HarnessEvent::Progress { .. } => {}
            }
            if journal.events.len() >= 256 {
                return Err(Error::invalid());
            }
            journal.events.push(value.clone());
        }
        let task_id = journal.task_id.clone();
        self.store
            .plugin_set("harness".into(), binding.id.clone(), json!(journal))
            .await
            .map_err(failure)?;
        Ok(task_id)
    }
}
