use crate::device::DeviceCommands as _;
use crate::schedule::ScheduleCommands as _;
use crate::work::WorkCommands as _;

use super::*;

pub(super) fn dialogue_tools(trigger: Trigger) -> Result<Vec<ToolDefinition>> {
    [
        ("work", "List delegated work, start or update it, or stop a session", json!({"action":{"enum":["list","start","update","stop"]},"session":{"type":"string"},"instruction":{"type":"string"}})),
        ("schedule", "Create, list or delete once-only and cron schedules", json!({"action":{"enum":["create","list","delete"]},"id":{"type":"string"},"instruction":{"type":"string"},"at":{"type":"string"},"cron":{"type":"string"},"zone":{"type":"string"}})),
        ("memory", "Search, remember, correct, forget, share or erase memories", json!({"action":{"enum":["search","remember","correct","forget","share","erase"]},"id":{"type":"string"},"revision":{"type":"integer"},"text":{"type":"string"},"importance":{"type":"integer","minimum":0,"maximum":10},"shared":{"type":"boolean"}})),
        ("policy", "List pending cards or resolve a directly stated approval. Include exact words from the latest user input.", json!({"action":{"enum":["list","allow","deny","add_text"]},"id":{"type":"string"},"quote":{"type":"string"},"text":{"type":"string"}})),
        ("body", "Request a gesture or expression for this companion", json!({"capability":{"enum":["body.motion","body.expression"]},"name":{"type":"string"}})),
    ].into_iter().filter(|(name, _, _)| {
        match trigger {
            Trigger::Event => *name == "body",
            Trigger::Schedule => matches!(*name, "work" | "body"),
            _ => true,
        }
    }).map(|(name, description, properties)| Ok(ToolDefinition::new(
        ToolName::new(name)?, description, json!({"type":"object","properties":properties,"additionalProperties":false}),
    ))).collect()
}

pub(super) async fn execute_tool(
    host: Host,
    dialogue: std::sync::Arc<Dialogue>,
    call: ToolCall,
) -> Result<String> {
    let input = call.function.arguments;
    let name = call.function.name;
    host.call(move |actor| {
        Box::pin(async move {
            if dialogue.cancel.is_cancelled()
                || !actor
                    .turns
                    .get(&dialogue.companion)
                    .is_some_and(|turn| turn.id == dialogue.turn)
            {
                return Err(invalid("dialogue turn is no longer active"));
            }
            let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
            if matches!(name.as_str(), "work" | "policy" | "schedule" | "memory")
                && !actor.claim(&dialogue)
            {
                return Err(invalid("another companion owns this utterance"));
            }
            match name.as_str() {
            "work" if text("action") == "list" => {
                let mut db = actor.db.acquire().await?;
                let sessions = store::list::<w::WorkSession>(&mut db, "work").await?;
                let summary: Vec<_> = sessions.into_iter().filter(|work| work.companion_id == dialogue.companion)
                    .rev().take(20).map(|work| {
                        let recent = work.runs.last().map(|run| json!({
                            "run": run.id, "outcome": run.outcome, "instruction": truncate(&run.instruction, 500),
                            "report": truncate(&run.report, 500)
                        }));
                        json!({"session": work.id, "recent": recent})
                    }).collect();
                Ok(serde_json::to_string(&summary)?)
            }
                "work" if text("action") == "stop" => {
                    actor
                        .stop_session(text("session"), Some(&dialogue.companion))
                        .await?;
                    Ok("Work stopped.".into())
                }
                "work" => {
                    if dialogue.scheduled {
                        let companion = actor.companion(&dialogue.companion).await?;
                        if let Err(error) = actor.selection(&companion, model_selection::Role::Work).await {
                            dialogue.deferred.store(true, std::sync::atomic::Ordering::Relaxed);
                            return Err(error);
                        }
                    }
                    let receipt = actor
                        .send_work(w::WorkInput {
                            mutation: if dialogue.scheduled {
                                Some(w::Mutation {
                                    request_key: crate::schedule::work_key(&dialogue.turn),
                                    ..Default::default()
                                }).into()
                            } else {
                                mutation()
                            },
                            companion_id: dialogue.companion.clone(),
                            session_id: text("session").into(),
                            instruction: text("instruction").into(),
                            ..Default::default()
                        })
                        .await?;
                    Ok(serde_json::to_string(&receipt)?)
                }
                "schedule" => actor.schedule_tool(&dialogue.companion, &input).await,
                "memory" => {
                    let query = text("text");
                    if text("action") == "search" {
                        return Ok(serde_json::to_string(
                            &actor
                                .search_memories(w::MemoryQuery {
                                    companion_id: dialogue.companion.clone(),
                                    query: query.into(),
                                    limit: 10,
                                    ..Default::default()
                                })
                                .await?,
                        )?);
                    }
                    let operation = match text("action") {
                        "remember" => w::memory_change::Operation::Remember,
                        "correct" => w::memory_change::Operation::Correct,
                        "forget" => w::memory_change::Operation::Forget,
                        "share" => w::memory_change::Operation::Share,
                        "erase" => w::memory_change::Operation::Erase,
                        _ => return Err(invalid("invalid memory action")),
                    };
                    if dialogue.direct.is_none() {
                        return Err(invalid("memory changes require user dialogue"));
                    }
                    let memory = w::Memory {
                        id: text("id").into(),
                        companion_id: dialogue.companion.clone(),
                        text: query.into(),
                        provenance: format!("dialogue:{}", dialogue.turn),
                        importance: input
                            .get("importance")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok())
                            .unwrap_or(5),
                        shared: input
                            .get("shared")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        ..Default::default()
                    };
                    Ok(serde_json::to_string(
                        &actor
                            .change_memory(w::MemoryChange {
                                operation: operation.into(),
                                memory: Some(memory).into(),
                                mutation: Some(w::Mutation {
                                    request_key: id(),
                                    revision: input
                                        .get("revision")
                                        .and_then(Value::as_i64)
                                        .unwrap_or(0),
                                    ..Default::default()
                                })
                                .into(),
                                ..Default::default()
                            })
                            .await?,
                    )?)
                }
                "policy" => crate::approval::policy_tool(actor, &dialogue, &input).await,
                "body" => {
                    actor
                        .gesture(&dialogue.companion, text("capability"), text("name"))
                        .await?;
                    Ok("Gesture queued.".into())
                }
                _ => Err(invalid("unknown dialogue tool")),
            }
        })
    })
    .await
}
