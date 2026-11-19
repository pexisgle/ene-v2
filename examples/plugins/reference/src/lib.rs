//! Deterministic contract fixtures, not a production task engine.
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

    thread_local! {
        static STEP: RefCell<Step> = const { RefCell::new(Step::Idle) };
        static CWD: RefCell<String> = const { RefCell::new(String::new()) };
    }
    /// The harness session state a real guest derives from the turn inputs:
    /// `Start` opens (or reopens) the session, `Prompt` continues the live
    /// one, and a turn parked on a permission request answers `Respond`.
    #[derive(Clone, Copy, PartialEq)]
    enum Step {
        /// No harness session: only `Start` is valid.
        Idle,
        /// A session exists with no turn in flight: `Prompt` continues it.
        Live,
        /// A turn is parked on the permission request: `Respond` or `Cancel`.
        AwaitingPermission,
    }

    const PERMISSION: &str = "permission-1";

    fn text_of(prompt: &[ContentBlock]) -> String {
        prompt
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn harness(input: &HarnessInput) -> Result<HarnessEvent, extism_pdk::Error> {
        Ok(match input {
            HarnessInput::Start { prompt, cwd } => {
                // The host opens a session only when it holds none, so a
                // second `Start` begins a fresh session rather than failing.
                STEP.with(|step| *step.borrow_mut() = Step::AwaitingPermission);
                CWD.with(|recorded| *recorded.borrow_mut() = cwd.clone());
                let instruction = text_of(prompt);
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
            HarnessInput::Prompt { prompt } => {
                STEP.with(|step| {
                    if *step.borrow() != Step::Live {
                        return Err(extism_pdk::Error::msg("no harness session"));
                    }
                    Ok(())
                })?;
                let follow_up = text_of(prompt);
                let cwd = CWD.with(|recorded| recorded.borrow().clone());
                let mut chunk = ContentChunk::new(text_block(format!(
                    "追加指示「{follow_up}」を受け付けました。作業ディレクトリは {cwd} です。"
                )));
                chunk.message_id = Some("follow-up".into());
                emit_update(SessionUpdate::AgentMessageChunk(chunk))?;
                HarnessEvent::Stop {
                    stop_reason: StopReason::EndTurn,
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
                    *step.borrow_mut() = Step::Live;
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
                // A cancelled turn leaves a live session, as ACP does.
                STEP.with(|step| {
                    let mut step = step.borrow_mut();
                    if *step != Step::Idle {
                        *step = Step::Live;
                    }
                });
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
            Operation::Hook { event } => {
                let decision = match event {
                    HookEvent::PreTurn { query, .. } => {
                        let cfg = configuration().unwrap_or(Value::Null);
                        let note = cfg
                            .get("inject_note")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !note.is_empty() {
                            HookDecision::InjectContext {
                                text: note.to_owned(),
                            }
                        } else if query
                            .as_deref()
                            .is_some_and(|q| q.contains("FORBIDDEN_TURN"))
                        {
                            HookDecision::Block {
                                reason: "blocked by reference hook".into(),
                            }
                        } else {
                            HookDecision::Continue
                        }
                    }
                    HookEvent::PreTool {
                        tool_name,
                        arguments,
                        ..
                    } => {
                        if arguments.get("block_me").and_then(Value::as_bool) == Some(true) {
                            HookDecision::Block {
                                reason: format!("hook blocked `{tool_name}`"),
                            }
                        } else {
                            HookDecision::Continue
                        }
                    }
                    _ => HookDecision::Continue,
                };
                return Ok(Json(serde_json::to_value(decision)?));
            }
            Operation::ListTools => {
                let defs = vec![PluginToolDefinition {
                    name: "reference_echo".into(),
                    description: "Echo input text through the reference plugin tool.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" }
                        },
                        "required": ["text"]
                    }),
                    read_only: true,
                }];
                return Ok(Json(serde_json::to_value(defs)?));
            }
            Operation::InvokeTool { request } => {
                let text = request
                    .arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let _ = log(format!("reference_echo: {text}"));
                let result = ToolInvokeResult {
                    output: json!({ "echo": text }),
                    is_error: false,
                };
                return Ok(Json(serde_json::to_value(result)?));
            }
            _ => return Err(extism_pdk::Error::msg("unsupported operation").into()),
        }
        Ok(Json(json!({})))
    }
}
