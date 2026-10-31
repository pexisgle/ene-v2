//! Deterministic contract fixtures, not production speech or autonomous task engines.
#[cfg(target_family = "wasm")]
mod guest {
    use ene_plugin::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    thread_local! { static PENDING: RefCell<Option<String>> = const { RefCell::new(None) }; }

    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<Value>> {
        Ok(Json(
            json!({"events":true,"state":true,"consultation":true,"cancellation":true,"formats":["pcm_s16le"]}),
        ))
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
                let event = PENDING.with(|pending| -> Result<HarnessEvent, extism_pdk::Error> {
                    let mut pending = pending.borrow_mut();
                    Ok(match input {
                        HarnessInput::Cancel => {
                            *pending = None;
                            HarnessEvent::Cancelled
                        }
                        HarnessInput::Start { instruction } => {
                            if pending.is_some() {
                                return Err(extism_pdk::Error::msg("task already active"));
                            }
                            *pending = Some("model-1".into());
                            HarnessEvent::ModelRequest {
                                operation_id: "model-1".into(),
                                request: GenerationRequest {
                                    endpoint: "https://example.com/api".into(),
                                    options: json!({}),
                                    model: "reference".into(),
                                    preamble: String::new(),
                                    messages: vec![provider::ChatMessage {
                                        role: "user".into(),
                                        text: instruction,
                                    }],
                                    max_tokens: 64,
                                    tools: vec![],
                                },
                            }
                        }
                        HarnessInput::Result { operation_id, .. } => {
                            if pending.as_ref() != Some(&operation_id) {
                                return Err(extism_pdk::Error::msg("wrong operation result"));
                            }
                            if operation_id == "model-1" {
                                *pending = Some("tool-1".into());
                                HarnessEvent::ToolRequest {
                                    operation_id: "tool-1".into(),
                                    name: "reference.inspect".into(),
                                    arguments: json!({}),
                                }
                            } else {
                                *pending = Some("question-1".into());
                                HarnessEvent::Question {
                                    question_id: "question-1".into(),
                                    text: "参照タスクを完了しますか？".into(),
                                }
                            }
                        }
                        HarnessInput::Answer {
                            question_id,
                            answer,
                        } => {
                            if pending.as_ref() != Some(&question_id) {
                                return Err(extism_pdk::Error::msg("wrong consultation"));
                            }
                            *pending = None;
                            HarnessEvent::Completed { report: answer }
                        }
                    })
                })?;
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
