use super::*;
pub async fn accept_message(
    app: HostApplication,
    id: String,
    key: String,
    request: MessageInput,
) -> Result<(Outcome, Value), Error> {
    if request.text.trim().is_empty() || request.text.len() > MAX_INPUT_BYTES {
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
        if receipt.0 != Outcome::Accepted {
            return Err(Error::new(ErrorKind::StorageFailure));
        }
        let accepted: Acceptance = serde_json::from_value(receipt.1.clone())
            .map_err(|_| Error::new(ErrorKind::StorageFailure))?;
        for id in [
            accepted.request_id,
            accepted.message_id,
            accepted.response_message_id,
        ] {
            MessageId::parse(id).map_err(|_| Error::new(ErrorKind::StorageFailure))?;
        }
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
    controls.admit(&app)?;
    let context = authorize(&app, &c, &controls, &id, &p, OperationKind::Conversation)?;
    crate::application::commit::accept_conversation(
        &app,
        &mut controls,
        p,
        context,
        id,
        key,
        body,
        request.text,
    )
    .await
}
