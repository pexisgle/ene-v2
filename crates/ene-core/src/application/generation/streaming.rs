use super::*;
async fn update(
    app: &HostApplication,
    id: &str,
    status: MessageStatus,
    delta: Option<String>,
    error: Option<Failure>,
) -> Result<(), Error> {
    let controls = app.0.controls.lock().await;
    if status == MessageStatus::Streaming
        && controls.running.get(id).is_none_or(|r| *r.cancel.borrow())
    {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let status = if status == MessageStatus::Completed
        && controls.running.get(id).is_none_or(|r| *r.cancel.borrow())
    {
        MessageStatus::Cancelled
    } else {
        status
    };
    let mut boundary = app.0.boundary.lock().await;
    let saved_id = id.to_string();
    let saved_delta = delta.clone();
    let saved_status = status.clone();
    let message = app
        .0
        .repository
        .update_message(saved_id, saved_status, saved_delta, error)
        .await?;
    if let Some(delta) = delta {
        publish(
            &mut boundary,
            app,
            "message.delta",
            Some(&message.companion_id),
            json!({"message_id":id,"offset":message.text.chars().count()-delta.chars().count(),"delta":delta}),
        );
    } else {
        publish(
            &mut boundary,
            app,
            if status == MessageStatus::Streaming {
                "message.started"
            } else {
                "message.updated"
            },
            Some(&message.companion_id),
            json!(message),
        );
    }
    Ok(())
}
pub async fn generate(
    app: HostApplication,
    companion: String,
    prompt: String,
    id: String,
    mut cancelled: watch::Receiver<bool>,
    mut predecessor: Option<watch::Receiver<Option<Result<(), Error>>>>,
    done: watch::Sender<Option<Result<(), Error>>>,
) {
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
        let (c, p, secret) = {
            let mut controls = app.0.controls.lock().await;
            if controls
                .running
                .values()
                .any(|run| run.terminal_error.is_some())
            {
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
            let secret = permitted(&app, &c, &controls, &companion, &p, "conversation")
                .map_err(|_| "permission_required")?;
            let policy = app.0.providers.policy(&p).map_err(|_| "upstream_error")?;
            let run = controls.running.get_mut(&id).ok_or("cancelled")?;
            run.connection = p.id.clone();
            run.policy = policy;
            (c, p, secret)
        };
        let companion_config = c.companions.get(&companion).ok_or("companion_missing")?;
        update(&app, &id, MessageStatus::Streaming, None, None)
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
            role: "user".into(),
            text: prompt,
        });
        let model = companion_config
            .model_assignments
            .companion
            .as_ref()
            .ok_or("model_unset")?
            .model_id
            .clone();
        let mut request = Generation {
            endpoint: p.endpoint.clone(),
            options: p.options.clone(),
            model,
            preamble: companion_config.mind.clone(),
            messages,
            max_tokens: 2048,
        };
        request.bound_history().map_err(|_| "input_limit")?;
        let input = invocation(
            &app,
            p,
            secret,
            id.clone(),
            request,
            Operation::Conversation,
            provider_cancel,
        );
        let mut stream = app.0.providers.generate(input).await.map_err(|e| {
            if e.reason == Some("input_limit") {
                "input_limit"
            } else {
                "upstream_error"
            }
        })?;
        let mut bytes = 0;
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
                    if bytes > 128_000 {
                        return Err("output_limit");
                    }
                    update(&app, &id, MessageStatus::Streaming, Some(text), None)
                        .await
                        .map_err(|_| "storage_error")?;
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
        Ok::<_, &'static str>(())
    };
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        tokio::select! {biased;_=cancelled.changed()=>Err("cancelled"),_=shutdown.changed()=>Err("cancelled"),r=tokio::time::timeout(Duration::from_secs(180),work)=>r.unwrap_or(Err("upstream_timeout"))}
    };
    let result = if *cancelled.borrow() || *shutdown.borrow() {
        Err("cancelled")
    } else {
        result
    };
    let (status, error) = match result {
        Ok(()) => (MessageStatus::Completed, None),
        Err("cancelled") => (MessageStatus::Cancelled, Some(failure("cancelled"))),
        Err(code) => (MessageStatus::Failed, Some(failure(code))),
    };
    let mut persisted = Err(Error::new(ErrorKind::StorageFailure));
    for attempt in 0..3 {
        persisted = update(&app, &id, status.clone(), None, error.clone()).await;
        if persisted.is_ok() {
            break;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if let Err(error) = persisted {
        let mut controls = app.0.controls.lock().await;
        if let Some(run) = controls.running.get_mut(&id) {
            run.terminal_error = Some(error.clone());
        }
        // Preserve the unresolved run and fail its dependent turns visibly.
        done.send_replace(Some(Err(error)));
        let mut boundary = app.0.boundary.lock().await;
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
    if status == MessageStatus::Failed
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
                        role: "user".into(),
                        text: user,
                    });
                    messages.push(ChatMessage {
                        role: "assistant".into(),
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
