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
    if let Some((status, value)) = receipt(&tx, key, &target, &request)? {
        if status != 202 {
            return Err(crate::infrastructure::sqlite::Error::CorruptReceipt);
        }
        return Ok(Accepted::Replay {
            receipt: serde_json::from_value(value)
                .map_err(crate::infrastructure::sqlite::Error::from)?,
        });
    }
    let n: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM messages WHERE companion_id=?1",
            [companion],
            |r| r.get(0),
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let response_sequence = n
        .checked_add(1)
        .filter(|_| n > 0)
        .ok_or(crate::infrastructure::sqlite::Error::ResourceLimit)?;
    let receipt = Acceptance {
        request_id: Uuid::new_v4().to_string(),
        message_id: Uuid::new_v4().to_string(),
        response_message_id: Uuid::new_v4().to_string(),
        ..Default::default()
    };
    let user = Message {
        id: receipt.message_id.clone(),
        companion_id: companion.into(),
        role: "user".into(),
        text: request["text"]
            .as_str()
            .ok_or(crate::infrastructure::sqlite::Error::StateConflict)?
            .into(),
        status: "completed".into(),
        created_at: now(),
        sequence: n as u64,
        resources: vec![],
        in_reply_to: None,
        task_id: None,
        question_id: None,
        failure: Default::default(),
        ..Default::default()
    };
    let response = Message {
        id: receipt.response_message_id.clone(),
        role: "companion".into(),
        text: String::new(),
        status: "queued".into(),
        sequence: response_sequence as u64,
        in_reply_to: Some(user.id.clone()),
        ..user.clone()
    };
    for m in [&user, &response] {
        let status = if m.role == "user" {
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
    Ok(Accepted::New {
        receipt,
        user: Box::new(user),
        response: Box::new(response),
    })
}

pub fn update_message(
    db: &Connection,
    id: &str,
    update: ene_core::domain::MessageUpdate,
) -> Result<Message, crate::infrastructure::sqlite::Error> {
    let body: String = db
        .query_row("SELECT body FROM messages WHERE id=?1", [id], |r| r.get(0))
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let mut message: Message =
        serde_json::from_str(&body).map_err(crate::infrastructure::sqlite::Error::from)?;
    if update.is_confirmed(&message) {
        return Ok(message);
    }
    update
        .apply(&mut message)
        .map_err(|_| crate::infrastructure::sqlite::Error::StateConflict)?;
    let status = serde_json::to_value(&message.status)
        .map_err(crate::infrastructure::sqlite::Error::from)?
        .as_str()
        .unwrap_or("failed")
        .to_string();
    let rows = db
        .execute(
            "UPDATE messages SET status=?1,body=?2 WHERE id=?3 AND body=?4",
            params![
                status,
                serde_json::to_string(&message)
                    .map_err(crate::infrastructure::sqlite::Error::from)?,
                id,
                body
            ],
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    if rows != 1 {
        return Err(crate::infrastructure::sqlite::Error::StateConflict);
    }
    Ok(message)
}
