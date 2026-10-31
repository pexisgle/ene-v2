//! The Host owns task identity, pending operations, consultation and final reports.
use super::*;
use ene_plugin::{HarnessEvent, HarnessInput};
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Journal {
    task_id: String,
    status: String,
    pending: Option<String>,
    question: bool,
    events: Vec<Event>,
    report: Option<String>,
}
impl Manager {
    pub(super) async fn harness_stop(&self, binding_id: &str, reason: &str) -> Result<()> {
        let value = self
            .store
            .plugin_get("harness".into(), binding_id.into())
            .await
            .map_err(storage_failure)?;
        if !value.is_null() {
            let journal: Journal = serde_json::from_value(value).map_err(storage_failure)?;
            if !matches!(
                journal.status.as_str(),
                "completed" | "failed" | "cancelled"
            ) {
                self.harness_commit(
                    binding_id,
                    journal,
                    &[Event::Harness {
                        event: HarnessEvent::Failed {
                            reason: reason.into(),
                        },
                    }],
                )
                .await?;
            }
        }
        Ok(())
    }
    pub(super) async fn harness_begin(
        &self,
        binding: &PluginBinding,
        input: &HarnessInput,
    ) -> Result<Journal> {
        let value = self
            .store
            .plugin_get("harness".into(), binding.id.clone())
            .await
            .map_err(storage_failure)?;
        let old = if value.is_null() {
            None
        } else {
            Some(serde_json::from_value::<Journal>(value).map_err(storage_failure)?)
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
                .filter(|j| {
                    j.status == "waiting_answer"
                        && j.question
                        && j.pending.as_ref() == Some(question_id)
                })
                .ok_or_else(conflict),
            HarnessInput::Result { operation_id, .. } => old
                .filter(|j| {
                    j.status == "waiting_operation"
                        && !j.question
                        && j.pending.as_ref() == Some(operation_id)
                })
                .ok_or_else(conflict),
        }
    }
    pub(super) async fn harness_cancel(
        &self,
        binding: &PluginBinding,
        journal: Journal,
        events: &[Event],
    ) -> Result<String> {
        if journal.status == "cancelled" {
            return Ok(journal.task_id);
        }
        self.harness_commit(&binding.id, journal, events).await
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
        self.harness_commit(&binding.id, journal, &[Event::Harness { event }])
            .await
    }
    pub(super) async fn harness_commit(
        &self,
        binding_id: &str,
        mut journal: Journal,
        events: &[Event],
    ) -> Result<String> {
        apply_events(&mut journal, events)?;
        let task_id = journal.task_id.clone();
        self.store
            .plugin_set("harness".into(), binding_id.into(), json!(journal))
            .await
            .map_err(storage_failure)?;
        Ok(task_id)
    }
}

fn apply_events(journal: &mut Journal, events: &[Event]) -> Result<()> {
    journal.pending = None;
    journal.question = false;
    let mut transitioned = false;
    for value in events {
        let Event::Harness { event } = value else {
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
                if journal.pending.replace(operation_id.clone()).is_some() {
                    return Err(conflict());
                }
                journal.status = "waiting_operation".into();
                transitioned = true;
            }
            HarnessEvent::Question { question_id, .. } => {
                if journal.pending.replace(question_id.clone()).is_some() {
                    return Err(conflict());
                }
                journal.question = true;
                journal.status = "waiting_answer".into();
                transitioned = true;
            }
            HarnessEvent::Completed { report } => {
                journal.status = "completed".into();
                journal.report = Some(report.clone());
                journal.pending = None;
                journal.question = false;
                transitioned = true;
            }
            HarnessEvent::Failed { reason } => {
                journal.status = "failed".into();
                journal.report = Some(reason.clone());
                journal.pending = None;
                journal.question = false;
                transitioned = true;
            }
            HarnessEvent::Cancelled => {
                journal.status = "cancelled".into();
                journal.pending = None;
                journal.question = false;
                transitioned = true;
            }
            HarnessEvent::Progress { .. } => {}
        }
        if journal.events.len() >= MAX_EVENTS {
            if !matches!(
                journal.status.as_str(),
                "completed" | "failed" | "cancelled"
            ) {
                return Err(Error::invalid());
            }
            // A full journal must still record the task's terminal state.
            journal.events.truncate(MAX_EVENTS - 1);
        }
        journal.events.push(value.clone());
    }
    if !transitioned {
        return Err(Error::invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_plugin::GenerationRequest;

    fn journal() -> Journal {
        Journal {
            task_id: "task".into(),
            status: "running".into(),
            pending: None,
            question: false,
            events: Vec::new(),
            report: None,
        }
    }
    fn progress() -> Event {
        Event::Harness {
            event: HarnessEvent::Progress {
                text: "step".into(),
            },
        }
    }
    fn request(id: &str) -> Event {
        Event::Harness {
            event: HarnessEvent::ModelRequest {
                operation_id: id.into(),
                request: GenerationRequest {
                    endpoint: String::new(),
                    options: serde_json::json!({}),
                    model: "model".into(),
                    preamble: String::new(),
                    messages: Vec::new(),
                    max_tokens: 1,
                    tools: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn harness_requires_a_state_changing_event() {
        let mut journal = journal();
        assert!(apply_events(&mut journal, &[]).is_err());
        assert!(apply_events(&mut journal, &[progress()]).is_err());
    }

    #[test]
    fn terminal_events_clear_pending_and_question() {
        let mut journal = journal();
        apply_events(
            &mut journal,
            &[
                request("model-1"),
                Event::Harness {
                    event: HarnessEvent::Completed {
                        report: "done".into(),
                    },
                },
            ],
        )
        .unwrap();
        assert_eq!(journal.status, "completed");
        assert!(journal.pending.is_none());
        assert!(!journal.question);
    }

    #[test]
    fn question_then_terminal_does_not_leave_an_answerable_state() {
        let mut journal = journal();
        apply_events(
            &mut journal,
            &[
                Event::Harness {
                    event: HarnessEvent::Question {
                        question_id: "q".into(),
                        text: "?".into(),
                    },
                },
                Event::Harness {
                    event: HarnessEvent::Failed { reason: "x".into() },
                },
            ],
        )
        .unwrap();
        assert_eq!(journal.status, "failed");
        assert!(journal.pending.is_none());
        assert!(!journal.question);
    }
}
