//! Persisted configuration, acceptance receipts and message state transitions.
//! Called on the database thread; no external service is called in a transaction.
use ene_core::domain::{Failure, Message, MessageStatus, Role};
use serde_json::{Value, json};
use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub use ene_core::domain::{Acceptance, Accepted, Configuration};

pub fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp")
}

pub fn load(db: &Connection) -> Result<Configuration, crate::infrastructure::sqlite::Error> {
    fn get<T: serde::de::DeserializeOwned>(
        db: &Connection,
        key: &str,
    ) -> Result<T, crate::infrastructure::sqlite::Error> {
        let value: String = db
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .map_err(crate::infrastructure::sqlite::Error::from)?;
        serde_json::from_str(&value).map_err(crate::infrastructure::sqlite::Error::from)
    }
    let companion = get(db, "companion")?;
    let settings = get(db, "settings")?;
    let extras: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='configuration_extras'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let value: Value = extras
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(crate::infrastructure::sqlite::Error::from)?
        .unwrap_or(json!({}));
    Ok(Configuration {
        companion,
        settings,
        grants: serde_json::from_value(value.get("grants").cloned().unwrap_or(json!([])))
            .map_err(crate::infrastructure::sqlite::Error::from)?,
        credential_refs: serde_json::from_value(
            value.get("credential_refs").cloned().unwrap_or(json!({})),
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?,
    })
}

pub fn write_configuration(
    db: &Connection,
    c: &Configuration,
) -> Result<(), crate::infrastructure::sqlite::Error> {
    for (key, value) in [
        (
            "companion",
            serde_json::to_value(&c.companion)
                .map_err(crate::infrastructure::sqlite::Error::from)?,
        ),
        (
            "settings",
            serde_json::to_value(&c.settings)
                .map_err(crate::infrastructure::sqlite::Error::from)?,
        ),
        (
            "configuration_extras",
            json!({"grants":c.grants,"credential_refs":c.credential_refs}),
        ),
    ] {
        db.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value.to_string()]).map_err(crate::infrastructure::sqlite::Error::from)?;
    }
    Ok(())
}

pub fn receipt(
    db: &Connection,
    key: &str,
    target: &str,
    request: &Value,
) -> Result<Option<(u16, Value)>, crate::infrastructure::sqlite::Error> {
    let principal: String = db
        .query_row(
            "SELECT value FROM metadata WHERE key='principal'",
            [],
            |r| r.get(0),
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let row:Option<(String,String,String,i64,String)>=db.query_row("SELECT method,target,request,status,response FROM receipts WHERE principal=?1 AND key=?2",params![principal,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(crate::infrastructure::sqlite::Error::from)?;
    match row {
        Some((method, old_target, body, status, response)) => {
            if method != "POST"
                || old_target != target
                || serde_json::from_str::<Value>(&body)
                    .map_err(crate::infrastructure::sqlite::Error::from)?
                    != *request
            {
                return Err(crate::infrastructure::sqlite::Error::IdempotencyConflict);
            }
            Ok(Some((
                status as u16,
                serde_json::from_str(&response)
                    .map_err(crate::infrastructure::sqlite::Error::from)?,
            )))
        }
        None => Ok(None),
    }
}

pub fn save_receipt(
    db: &Connection,
    key: &str,
    target: &str,
    request: &Value,
    status: u16,
    response: &Value,
) -> Result<(), crate::infrastructure::sqlite::Error> {
    db.execute("INSERT INTO receipts SELECT value,?1,'POST',?2,?3,?4,?5 FROM metadata WHERE key='principal'",params![key,target,request.to_string(),status,response.to_string()]).map_err(crate::infrastructure::sqlite::Error::from)?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sqlite::Store;
    #[tokio::test]
    async fn acceptance_replay_conflict_and_storage_failure_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).await.unwrap();
        store.run(|conn| {
            let c=load(conn)?.companion.id;
            let body=json!({"text":"日本🙂"});
            let accepted=accept(conn,"key",&c,body.clone())?;
            let replay=accept(conn,"key",&c,body)?;
            assert_eq!(accepted.receipt.request_id,replay.receipt.request_id);
            assert_eq!(accepted.receipt.response_message_id,replay.receipt.response_message_id);
            assert!(!replay.created);
            assert!(accept(conn,"key",&c,json!({"text":"別の本文"})).is_err());
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM messages",[],|r|r.get::<_,i64>(0)).unwrap(),2);
            conn.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(FAIL,'disk full'); END;").unwrap();
            assert!(accept(conn,"failed-key",&c,json!({"text":"書込失敗"})).is_err());
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM messages",[],|r|r.get::<_,i64>(0)).unwrap(),2);
            Ok(())
        }).await.unwrap();
    }
    #[tokio::test]
    async fn accepted_request_is_not_reexecuted_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let first = Store::open(path.clone()).await.unwrap();
        let id = first
            .run(|conn| {
                let c = load(conn)?.companion.id;
                Ok(accept(conn, "key", &c, json!({"text":"再起動"}))?
                    .receipt
                    .response_message_id)
            })
            .await
            .unwrap();
        // Opening another database actor models a host restart; startup fails queued output.
        let second = Store::open(path).await.unwrap();
        let replay = second
            .run(|conn| {
                let c = load(conn)?.companion.id;
                accept(conn, "key", &c, json!({"text":"再起動"}))
            })
            .await
            .unwrap();
        assert_eq!(id, replay.receipt.response_message_id);
        assert!(!replay.created);
        let c = second.snapshot().await.unwrap().companion.id;
        let message = second.message(c, id).await.unwrap().unwrap();
        assert_eq!(message.status, MessageStatus::Failed);
        assert_eq!(message.failure.unwrap().code, "host_interrupted");
    }
}

#[cfg(test)]
mod recovery_tests {
    #[tokio::test]
    async fn interrupted_check_is_final_and_a_replay_never_reexecutes_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.db");
        let store = crate::infrastructure::sqlite::Store::open(path.clone())
            .await
            .unwrap();
        store
            .run(|db| {
                super::save_receipt(
                    db,
                    "check",
                    "/v1/connections/p/check",
                    &serde_json::json!({"purpose":"companion"}),
                    0,
                    &serde_json::json!({"request_id":"original"}),
                )
            })
            .await
            .unwrap();
        let restarted = crate::infrastructure::sqlite::Store::open(path)
            .await
            .unwrap();
        let replay = restarted
            .run(|db| {
                super::receipt(
                    db,
                    "check",
                    "/v1/connections/p/check",
                    &serde_json::json!({"purpose":"companion"}),
                )
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.0, 503);
        assert_eq!(replay.1["request_id"], "original");
        assert_eq!(replay.1["error"]["details"]["reason"], "host_interrupted");
    }
}
