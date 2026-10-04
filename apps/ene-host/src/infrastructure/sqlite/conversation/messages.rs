use super::*;

pub fn accept(
    db: &mut Connection,
    key: &str,
    companion: &str,
    request: Value,
) -> Result<Accepted, crate::infrastructure::sqlite::Error> {
    let target = format!("/v1/companions/{companion}/messages");
    let tx = db
        .transaction()
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    if let Some((_, value)) = receipt(&tx, key, &target, &request)? {
        return Ok(Accepted {
            receipt: serde_json::from_value(value)
                .map_err(crate::infrastructure::sqlite::Error::from)?,
            created: false,
            user: None,
            response: None,
        });
    }
    let n: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM messages WHERE companion_id=?1",
            [companion],
            |r| r.get(0),
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let receipt = Acceptance {
        request_id: Uuid::new_v4().to_string(),
        message_id: Uuid::new_v4().to_string(),
        response_message_id: Uuid::new_v4().to_string(),
    };
    let user = Message {
        id: receipt.message_id.clone(),
        companion_id: companion.into(),
        role: Role::User,
        text: request["text"]
            .as_str()
            .ok_or(crate::infrastructure::sqlite::Error::StateConflict)?
            .into(),
        status: MessageStatus::Completed,
        created_at: now(),
        sequence: n as u64,
        resources: vec![],
        in_reply_to: None,
        task_id: None,
        question_id: None,
        failure: None,
    };
    let response = Message {
        id: receipt.response_message_id.clone(),
        role: Role::Companion,
        text: String::new(),
        status: MessageStatus::Queued,
        sequence: (n + 1) as u64,
        in_reply_to: Some(user.id.clone()),
        ..user.clone()
    };
    for m in [&user, &response] {
        let status = if m.role == Role::User {
            "completed"
        } else {
            "queued"
        };
        tx.execute(
            "INSERT INTO messages VALUES (?1,?2,?3,?4,?5)",
            params![
                m.id,
                m.companion_id,
                m.sequence as i64,
                status,
                serde_json::to_string(m).map_err(crate::infrastructure::sqlite::Error::from)?
            ],
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    }
    save_receipt(
        &tx,
        key,
        &target,
        &request,
        202,
        &serde_json::to_value(&receipt).map_err(crate::infrastructure::sqlite::Error::from)?,
    )?;
    tx.commit()
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    Ok(Accepted {
        receipt,
        created: true,
        user: Some(user),
        response: Some(response),
    })
}

pub fn update_message(
    db: &Connection,
    id: &str,
    status: MessageStatus,
    delta: Option<String>,
    failure: Option<Failure>,
) -> Result<Message, crate::infrastructure::sqlite::Error> {
    let body: String = db
        .query_row("SELECT body FROM messages WHERE id=?1", [id], |r| r.get(0))
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let mut message: Message =
        serde_json::from_str(&body).map_err(crate::infrastructure::sqlite::Error::from)?;
    ene_core::domain::transition(&mut message, status, delta, failure)
        .map_err(|_| crate::infrastructure::sqlite::Error::StateConflict)?;
    let status = serde_json::to_value(&message.status)
        .map_err(crate::infrastructure::sqlite::Error::from)?
        .as_str()
        .unwrap_or("failed")
        .to_string();
    db.execute(
        "UPDATE messages SET status=?1,body=?2 WHERE id=?3",
        params![
            status,
            serde_json::to_string(&message).map_err(crate::infrastructure::sqlite::Error::from)?,
            id
        ],
    )
    .map_err(crate::infrastructure::sqlite::Error::from)?;
    Ok(message)
}
