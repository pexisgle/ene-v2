use super::*;
pub async fn check(
    app: HostApplication,
    id: String,
    key: String,
    request: CheckInput,
) -> Result<(Outcome, Value), Error> {
    use crate::ports::llm::{ChatMessage, Event, Generation};
    use futures_util::StreamExt;
    if !["companion", "task"].contains(&request.purpose.as_str()) {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let body = json!({"companion_id":request.companion_id,"purpose":request.purpose,"model_id":request.model_id});
    let target = format!("/v1/connections/{id}/check");
    let mut controls = app.0.controls.lock().await;
    if let Some((outcome, response)) = app
        .0
        .repository
        .receipt(key.clone(), target.clone(), body.clone())
        .await?
    {
        if outcome == Outcome::Pending {
            return Err(Error::new(ErrorKind::RequestInProgress));
        }
        return Ok((outcome, response));
    }
    let c = config(&app, &mut controls).await?;
    if !c.companions.contains_key(&request.companion_id) {
        return Err(Error::new(ErrorKind::NotFound));
    }
    let p = rules::connection(&c, &id).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    let prepared = prepare(
        &app,
        &c,
        &controls,
        &request.companion_id,
        &p,
        Operation::ConnectionCheck,
    )?;
    let companion = c
        .companions
        .get(&request.companion_id)
        .expect("validated companion");
    let assignment = if request.purpose == "task" {
        &companion.model_assignments.task
    } else {
        &companion.model_assignments.companion
    };
    let model = request
        .model_id
        .or_else(|| {
            assignment
                .as_ref()
                .filter(|a| a.connection_id == id)
                .map(|a| a.model_id.clone())
        })
        .or_else(|| {
            app.providers()
                .into_iter()
                .find(|d| d.id == p.adapter)
                .and_then(|d| d.models.first().map(|m| m.id.clone()))
        })
        .ok_or_else(Error::invalid)?;
    if model.trim().is_empty() || model.len() > 512 {
        return Err(Error::invalid());
    }
    let request_id = Uuid::new_v4().to_string();
    app.0
        .repository
        .reserve(Receipt {
            key: key.clone(),
            target: target.clone(),
            request: body.clone(),
            outcome: Outcome::Pending,
            response: json!({"request_id":request_id}),
        })
        .await?;
    let mut cancelled = prepared.cancelled;
    let secret = prepared.secret;
    controls
        .running
        .insert(request_id.clone(), prepared.running);
    drop(controls);
    let generation = Generation {
        endpoint: p.endpoint.clone(),
        options: p.options.clone(),
        model: model.clone(),
        preamble: String::new(),
        messages: vec![ChatMessage {
            role: "user".into(),
            text: "日本語で接続確認に成功したことを一文だけ返してください。".into(),
        }],
        max_tokens: 128,
    };
    let input = invocation(
        &app,
        p,
        secret,
        request_id.clone(),
        generation,
        cancelled.clone(),
    );
    let work = async {
        let mut stream = app.0.providers.generate(input).await?;
        let mut text = String::new();
        let mut chunks = 0;
        let mut finished = false;
        while let Some(event) = stream.next().await {
            match event? {
                Event::Delta(delta) => {
                    text.push_str(&delta);
                    chunks += 1;
                    if text.len() > 8192 {
                        return Err(Error::new(ErrorKind::ExternalFailure));
                    }
                }
                Event::Finished(reason) => {
                    finished = reason == "stop";
                    break;
                }
            }
        }
        if !finished || text.trim().is_empty() {
            return Err(Error::new(ErrorKind::ExternalFailure));
        }
        Ok((chunks, text.chars().count()))
    };
    let mut shutdown = app.0.shutdown.subscribe();
    let result = Operation::ConnectionCheck
        .wait(&mut cancelled, &mut shutdown, work)
        .await
        .map_err(|failure| match failure {
            RunFailure::Failed(error) => error,
            _ => Error::unavailable(),
        });
    let mut controls = app.0.controls.lock().await;
    let mut c = config(&app, &mut controls).await?;
    let cancelled_now = is_cancelled(&app, &controls, &request_id);
    let (outcome, response) = match result {
        Ok((chunks, scalars)) if !cancelled_now => {
            if let Some(mut p) = rules::connection(&c, &id) {
                p.capabilities = Some(json!({"streaming_text":true}));
                p.last_check = Some(
                    json!({"success":true,"model_id":model,"checked_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).map_err(|_|Error::unavailable())?}),
                );
                update_connection(&mut c, p);
            }
            (
                Outcome::Completed,
                json!({"request_id":request_id,"capabilities":{"streaming_text":true},"success":true,"model_id":model,"content_chunks":chunks,"unicode_scalars":scalars}),
            )
        }
        _ => (
            Outcome::Unavailable,
            json!({"request_id":request_id,"error":{"code":"unavailable","message":"接続確認を完了できませんでした","details":{}}}),
        ),
    };
    let persisted = crate::application::configuration::commit(
        &app,
        &mut controls,
        c,
        Some(crate::ports::ReceiptWrite::Complete(Receipt {
            key,
            target,
            request: body,
            outcome,
            response: response.clone(),
        })),
    )
    .await;
    settle(&app, &mut controls, &request_id, persisted).await?;
    Ok((outcome, response))
}
