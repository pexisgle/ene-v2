//! Deterministic contract fixtures, not production speech or autonomous task engines.
#[cfg(target_family = "wasm")]
mod guest {
    use ene_plugin::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;

    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<Value>> {
        Ok(Json(
            json!({"events":true,"state":true,"consultation":true,"cancellation":true,"formats":["pcm_s16le"]}),
        ))
    }

    thread_local! { static STEP: RefCell<Step> = const { RefCell::new(Step::Idle) }; }
    #[derive(Clone, Copy, PartialEq)]
    enum Step {
        Idle,
        AwaitingPermission,
    }

    const PERMISSION: &str = "permission-1";

    fn harness(input: &HarnessInput) -> Result<HarnessEvent, extism_pdk::Error> {
        Ok(match input {
            HarnessInput::Start { prompt } => {
                STEP.with(|step| {
                    if *step.borrow() != Step::Idle {
                        return Err(extism_pdk::Error::msg("task already active"));
                    }
                    *step.borrow_mut() = Step::AwaitingPermission;
                    Ok(())
                })?;
                let instruction = prompt
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(text) => text.text.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                emit_update(SessionUpdate::Plan(Plan {
                    entries: vec![
                        PlanEntry {
                            content: "参照データを確認する".into(),
                            priority: PlanEntryPriority::High,
                            status: PlanEntryStatus::InProgress,
                        },
                        PlanEntry {
                            content: "結果をまとめる".into(),
                            priority: PlanEntryPriority::Medium,
                            status: PlanEntryStatus::Pending,
                        },
                    ],
                }))?;
                emit_update(SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::text(format!(
                        "{instruction} の参照データを確認します。"
                    )),
                    message_id: Some("progress".into()),
                }))?;
                emit_update(SessionUpdate::ToolCall(ToolCall {
                    tool_call_id: "tool-1".into(),
                    title: "参照データを確認".into(),
                    name: Some("reference.inspect".into()),
                    kind: Some(ToolKind::Read),
                    ..Default::default()
                }))?;
                HarnessEvent::RequestPermission {
                    request_id: PERMISSION.into(),
                    tool_call: ToolCallUpdate {
                        tool_call_id: "tool-1".into(),
                        ..Default::default()
                    },
                    options: vec![
                        PermissionOption {
                            option_id: "allow".into(),
                            name: "許可する".into(),
                            kind: PermissionOptionKind::AllowOnce,
                        },
                        PermissionOption {
                            option_id: "deny".into(),
                            name: "拒否する".into(),
                            kind: PermissionOptionKind::RejectOnce,
                        },
                    ],
                }
            }
            HarnessInput::Respond {
                request_id,
                outcome,
            } => {
                STEP.with(|step| {
                    if *step.borrow() != Step::AwaitingPermission {
                        return Err(extism_pdk::Error::msg("no permission request"));
                    }
                    if request_id != PERMISSION {
                        return Err(extism_pdk::Error::msg("wrong permission request"));
                    }
                    *step.borrow_mut() = Step::Idle;
                    Ok(())
                })?;
                let allowed = matches!(
                    outcome,
                    RequestPermissionOutcome::Selected { option_id } if option_id == "allow"
                );
                if allowed {
                    emit_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: "tool-1".into(),
                        status: Some(ToolCallStatus::InProgress),
                        ..Default::default()
                    }))?;
                    emit_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: "tool-1".into(),
                        status: Some(ToolCallStatus::Completed),
                        content: Some(vec![ToolCallContent::Content {
                            content: ContentBlock::text("参照データを確認しました。"),
                        }]),
                        ..Default::default()
                    }))?;
                    emit_update(SessionUpdate::AgentMessageChunk(ContentChunk {
                        content: ContentBlock::text("参照タスクを完了しました。"),
                        message_id: Some("report".into()),
                    }))?;
                    HarnessEvent::Stop {
                        stop_reason: StopReason::EndTurn,
                    }
                } else {
                    HarnessEvent::Stop {
                        stop_reason: StopReason::Refusal,
                    }
                }
            }
            HarnessInput::Cancel => {
                STEP.with(|step| *step.borrow_mut() = Step::Idle);
                HarnessEvent::Stop {
                    stop_reason: StopReason::Cancelled,
                }
            }
        })
    }

    fn emit_update(update: SessionUpdate) -> Result<(), extism_pdk::Error> {
        emit(Event::Harness {
            event: HarnessEvent::Update { update },
        })
    }

    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Operation>) -> FnResult<Json<Value>> {
        match operation {
            Operation::Character { target, event } => {
                if matches!(
                    event,
                    CharacterEvent::Activated | CharacterEvent::ConversationFinished { .. }
                ) {
                    let count = service::<Option<u64>>(ServiceRequest::StateRead {
                        key: "visits".into(),
                    })?
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| extism_pdk::Error::msg("visit count overflow"))?;
                    service::<()>(ServiceRequest::StateWrite {
                        key: "visits".into(),
                        value: json!(count),
                    })?;
                    emit(Event::Character {
                        target,
                        action: CharacterAction::Expression {
                            name: "happy".into(),
                            duration_ms: 1500,
                        },
                    })?;
                    return Ok(Json(json!({"visits":count})));
                }
            }
            Operation::Harness { input } => {
                let event = harness(&input)?;
                emit(Event::Harness { event })?;
            }
            Operation::Speech { input } => emit(match input {
                SpeechInput::Synthesize {
                    utterance_id,
                    generation,
                    ..
                } => Event::Audio {
                    chunk: AudioChunk {
                        utterance_id,
                        generation,
                        sequence: 0,
                        format: "pcm_s16le".into(),
                        sample_rate: 16000,
                        channels: 1,
                        data_base64: "AAA=".into(),
                        done: true,
                    },
                },
                SpeechInput::Transcribe { .. } => Event::Transcript {
                    text: "reference transcript".into(),
                },
            })?,
            _ => return Err(extism_pdk::Error::msg("unsupported operation").into()),
        }
        Ok(Json(json!({})))
    }
}
