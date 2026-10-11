use super::*;
use crate::application::commit::update_message as update;
pub async fn generate(
    app: HostApplication,
    companion: String,
    prompt: String,
    id: String,
    mut cancelled: watch::Receiver<bool>,
    mut predecessor: Option<watch::Receiver<Option<Result<(), Error>>>>,
    done: watch::Sender<Option<Result<(), Error>>>,
) {
    app.character_event(&companion, json!({"type":"conversation_started"}));
    let provider_cancel = cancelled.clone();
    let waiting = async {
        if let Some(previous) = predecessor.as_mut() {
            loop {
                if let Some(result) = previous.borrow().clone() {
                    result.map_err(|_| "storage_error")?;
                    break;
                }
                previous.changed().await.map_err(|_| "storage_error")?;
            }
        }
        Ok::<_, &'static str>(())
    };
    let mut shutdown = app.0.shutdown.subscribe();
    let queued = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err("cancelled"),
            _ = shutdown.changed() => Err("cancelled"),
            result = waiting => result,
        }
    };
    let predecessor_failed = queued == Err("storage_error");
    let work = async {
        queued?;
        // Resolve execution settings/secrets now; acceptance only records cancellation scope.
        let (c, p, context) = {
            let mut controls = app.0.controls.lock().await;
            if controls.failed() {
                return Err("storage_error");
            }
            let c = config(&app, &mut controls)
                .await
                .map_err(|_| "storage_error")?;
            let companion_config = c.companions.get(&companion).ok_or("companion_missing")?;
            let assignment = companion_config
                .model_assignments
                .companion
                .as_ref()
                .ok_or("model_unset")?;
            let p = rules::connection(&c, &assignment.connection_id).ok_or("model_unset")?;
            let context = authorize(
                &app,
                &c,
                &controls,
                &companion,
                &p,
                OperationKind::Conversation,
            )
            .map_err(|_| "permission_required")?;
            let run = controls.running.get_mut(&id).ok_or("cancelled")?;
            if *run.cancel.borrow() {
                return Err("cancelled");
            }
            run.connection = p.id.clone();
            run.policy = context.binding.policy().clone();
            run.nonce = context.secret.as_ref().map(|secret| secret.nonce.clone());
            (c, p, context)
        };
        let companion_config = c.companions.get(&companion).ok_or("companion_missing")?;
        update(&app, &id, MessageUpdate::Start)
            .await
            .map_err(|_| "storage_error")?;
        let mut history = app
            .0
            .repository
            .messages(companion.clone(), None, 100)
            .await
            .map_err(|_| "storage_error")?
            .items;
        let response = history
            .iter()
            .find(|m| m.id == id)
            .ok_or("storage_error")?
            .sequence;
        history.retain(|m| m.sequence < response.saturating_sub(1));
        let mut messages = completed_turns(history);
        messages.push(ChatMessage {
            role: crate::ports::llm::ChatRole::User,
            text: prompt,
        });
        let model = companion_config
            .model_assignments
            .companion
            .as_ref()
            .ok_or("model_unset")?
            .model_id
            .clone();
        let request = Generation::new(&p, model, companion_config.mind.clone(), messages, 2048)
            .map_err(|_| "input_limit")?;
        let input = invocation(
            &app,
            p,
            context,
            id.clone(),
            InvocationInput::Conversation(request),
            provider_cancel,
        );
        let mut stream = input.generate().await.map_err(|e| {
            if e.reason == Some("input_limit") {
                "input_limit"
            } else {
                "upstream_error"
            }
        })?;
        let mut bytes = 0;
        let mut offset = 0;
        let mut finished = false;
        while let Some(event) = stream.next().await {
            match event.map_err(|e| {
                if e.reason == Some("input_limit") {
                    "input_limit"
                } else {
                    "upstream_error"
                }
            })? {
                Event::Delta(text) => {
                    bytes += text.len();
                    if bytes > OperationKind::Conversation.spec().output_bytes {
                        return Err("output_limit");
                    }
                    let scalars = text.chars().count();
                    update(&app, &id, MessageUpdate::Append { offset, text })
                        .await
                        .map_err(|_| "storage_error")?;
                    offset += scalars;
                }
                Event::Finished(reason) => {
                    if reason != "stop" {
                        return Err("incomplete_output");
                    }
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            return Err("incomplete_output");
        }
        if let Ok(Some(message)) = app
            .0
            .repository
            .message(companion.clone(), id.clone())
            .await
        {
            app.character_event(
                &companion,
                json!({"type":"conversation_finished","text":message.text}),
            );
        }
        Ok::<_, &'static str>(())
    };
    let result = execute(&app, OperationKind::Conversation, &mut cancelled, async {
        work.await.map_err(|code| Error {
            kind: if code == "cancelled" {
                ErrorKind::PermissionRequired
            } else {
                ErrorKind::ExternalFailure
            },
            reason: Some(code),
        })
    })
    .await;
    let terminal = match result {
        Ok(()) => MessageUpdate::Complete,
        Err(error) if error.kind == ErrorKind::PermissionRequired => {
            MessageUpdate::Cancel(failure("cancelled"))
        }
        Err(error) => MessageUpdate::Fail(failure(error.reason.unwrap_or("upstream_error"))),
    };
    let mut persisted = Err(Error::new(ErrorKind::StorageFailure));
    for attempt in 0..3 {
        persisted = update(&app, &id, terminal.clone()).await;
        if persisted.is_ok() {
            break;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if let Err(error) = persisted {
        let mut controls = app.0.controls.lock().await;
        // Preserve the unresolved run and fail its dependent turns visibly.
        done.send_replace(Some(Err(error)));
        let mut boundary = app.0.boundary.lock().await;
        controls.fail_storage(&app, &mut boundary);
        publish(
            &mut boundary,
            &app,
            "generation.persistence_failed",
            Some(&companion),
            json!({"message_id":id,"code":"storage_error"}),
        );
        return;
    }
    done.send_replace(Some(if predecessor_failed {
        Err(Error::new(ErrorKind::StorageFailure))
    } else {
        Ok(())
    }));
    let mut controls = app.0.controls.lock().await;
    controls.running.remove(&id);
    if matches!(terminal, MessageUpdate::Fail(_))
        && let Some(c) = controls.config.clone()
    {
        let _ = commit(&app, &mut controls, c).await;
    }
}

// A completed user row alone does not mean its request was sent or completed.
// Page boundaries, cancelled replies and task messages must not create partial turns.
fn completed_turns(history: Vec<Message>) -> Vec<ChatMessage> {
    let mut users = BTreeMap::new();
    let mut messages = Vec::new();
    for message in history {
        if message.status != MessageStatus::Completed || message.task_id.is_some() {
            continue;
        }
        match message.role {
            Role::User => {
                users.insert(message.id, message.text);
            }
            Role::Companion => {
                if let Some(user) = message.in_reply_to.and_then(|id| users.remove(&id)) {
                    messages.push(ChatMessage {
                        role: crate::ports::llm::ChatRole::User,
                        text: user,
                    });
                    messages.push(ChatMessage {
                        role: crate::ports::llm::ChatRole::Assistant,
                        text: message.text,
                    });
                }
            }
            _ => {}
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_contains_only_complete_conversation_pairs() {
        let row = |id: &str,
                   role: Role,
                   status: MessageStatus,
                   reply: Option<&str>,
                   task: Option<&str>| {
            serde_json::from_value::<Message>(json!({
                "id": id, "companion_id":"c", "role":role, "text":id,
                "status":status, "created_at":"now", "sequence":1,
                "in_reply_to":reply, "task_id":task, "question_id":null, "failure":null
            }))
            .unwrap()
        };
        let messages = completed_turns(vec![
            row(
                "page-orphan",
                Role::Companion,
                MessageStatus::Completed,
                Some("outside-page"),
                None,
            ),
            row(
                "cancelled-input",
                Role::User,
                MessageStatus::Completed,
                None,
                None,
            ),
            row(
                "cancelled",
                Role::Companion,
                MessageStatus::Cancelled,
                Some("cancelled-input"),
                None,
            ),
            row(
                "failed-input",
                Role::User,
                MessageStatus::Completed,
                None,
                None,
            ),
            row(
                "failed",
                Role::Companion,
                MessageStatus::Failed,
                Some("failed-input"),
                None,
            ),
            row(
                "task-input",
                Role::User,
                MessageStatus::Completed,
                None,
                Some("task"),
            ),
            row(
                "task-reply",
                Role::Companion,
                MessageStatus::Completed,
                Some("task-input"),
                Some("task"),
            ),
            row(
                "completed-input",
                Role::User,
                MessageStatus::Completed,
                None,
                None,
            ),
            row(
                "completed-reply",
                Role::Companion,
                MessageStatus::Completed,
                Some("completed-input"),
                None,
            ),
            row(
                "unanswered-input",
                Role::User,
                MessageStatus::Completed,
                None,
                None,
            ),
        ]);
        assert_eq!(
            messages
                .iter()
                .map(|m| (m.role.as_str(), m.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "completed-input"),
                ("assistant", "completed-reply")
            ]
        );
    }
}
