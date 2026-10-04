use super::*;
pub async fn accept_message(
    app: HostApplication,
    id: String,
    key: String,
    request: MessageInput,
) -> Result<(Outcome, Value), Error> {
    if request.text.trim().is_empty() || request.text.len() > 65_536 {
        return Err(Error::invalid());
    }
    if !request.resources.is_empty() || request.question_id.is_some() || request.decision.is_some()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let body = json!({"text":request.text,"resources":request.resources,"question_id":request.question_id,"decision":request.decision});
    let target = format!("/v1/companions/{id}/messages");
    let lookup = body.clone();
    let key_lookup = key.clone();
    let mut controls = app.0.controls.lock().await;
    if *app.0.shutdown.borrow() || app.0.generations.is_closed() {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    if let Some(receipt) = app.0.repository.receipt(key_lookup, target, lookup).await? {
        return Ok(receipt);
    }
    let c = config(&app, &mut controls).await?;
    let companion = c
        .companions
        .get(&id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let assignment = companion
        .model_assignments
        .companion
        .as_ref()
        .ok_or(Error {
            kind: ErrorKind::StateConflict,
            reason: Some("model_unset"),
        })?;
    let p = rules::connection(&c, &assignment.connection_id)
        .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
    permitted(&app, &c, &controls, &id, &p, "conversation")?;
    let policy = app.0.providers.policy(&p)?;
    if controls
        .running
        .values()
        .any(|r| r.terminal_error.is_some())
    {
        return Err(Error::new(ErrorKind::StorageFailure));
    }
    if controls.running.len() >= 8 {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let mut boundary = app.0.boundary.lock().await;
    let accepted = app.0.repository.accept(key, id.clone(), body).await?;
    if !accepted.created {
        return Ok((Outcome::Accepted, json!(accepted.receipt)));
    }
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&id),
        json!(accepted.user),
    );
    publish(
        &mut boundary,
        &app,
        "message.created",
        Some(&id),
        json!(accepted.response),
    );
    let (cancel, receiver) = watch::channel(false);
    let response_id = accepted.receipt.response_message_id.clone();
    let (done, completion) = watch::channel(None);
    let predecessor = controls.tails.insert(id.clone(), completion);
    controls.running.insert(
        response_id.clone(),
        Running {
            companion: id.clone(),
            connection: p.id.clone(),
            data: vec!["conversation"],
            policy,
            terminal_error: None,
            purpose: "conversation",
            cancel,
        },
    );
    app.0.generations.spawn(generate(
        app.clone(),
        id,
        request.text,
        response_id,
        receiver,
        predecessor,
        done,
    ));
    Ok((Outcome::Accepted, json!(accepted.receipt)))
}
