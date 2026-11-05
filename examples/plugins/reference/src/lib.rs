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
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                emit_update(SessionUpdate::Plan(Plan::new(vec![
                    PlanEntry::new(
                        "参照データを確認する",
                        PlanEntryPriority::High,
                        PlanEntryStatus::InProgress,
                    ),
                    PlanEntry::new(
                        "結果をまとめる",
                        PlanEntryPriority::Medium,
                        PlanEntryStatus::Pending,
                    ),
                ])))?;
                let mut chunk = ContentChunk::new(text_block(format!(
                    "{instruction} の参照データを確認します。"
                )));
                chunk.message_id = Some("progress".into());
                emit_update(SessionUpdate::AgentMessageChunk(chunk))?;
                emit_update(SessionUpdate::ToolCall(
                    ToolCall::new("tool-1", "参照データを確認")
                        .name("reference.inspect")
                        .kind(ToolKind::Read),
                ))?;
                HarnessEvent::RequestPermission {
                    request_id: PERMISSION.into(),
                    tool_call: ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new()),
                    options: vec![
                        PermissionOption::new("allow", "許可する", PermissionOptionKind::AllowOnce),
                        PermissionOption::new("deny", "拒否する", PermissionOptionKind::RejectOnce),
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
                    RequestPermissionOutcome::Selected(selected)
                        if selected.option_id.to_string() == "allow"
                );
                if allowed {
                    emit_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        "tool-1",
                        ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
                    )))?;
                    emit_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        "tool-1",
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .content(vec![ToolCallContent::Content(Content::new(text_block(
                                "参照データを確認しました。",
                            )))]),
                    )))?;
                    let mut chunk = ContentChunk::new(text_block("参照タスクを完了しました。"));
                    chunk.message_id = Some("report".into());
                    emit_update(SessionUpdate::AgentMessageChunk(chunk))?;
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
