use super::*;

pub(crate) fn principal(db: &Connection) -> Result<String, crate::infrastructure::sqlite::Error> {
    db.query_row(
        "SELECT value FROM metadata WHERE key='principal'",
        [],
        |r| r.get(0),
    )
    .map_err(crate::infrastructure::sqlite::Error::from)
}

/// A stored response may spell the request id `requestId` or `request_id`.
pub(crate) fn request_id(value: &Value) -> Option<&str> {
    value
        .get("requestId")
        .or_else(|| value.get("request_id"))
        .and_then(Value::as_str)
}

pub fn receipt(
    db: &Connection,
    key: &str,
    target: &str,
    request: &Value,
) -> Result<Option<(u16, Value)>, crate::infrastructure::sqlite::Error> {
    let principal = principal(db)?;
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
                u16::try_from(status)
                    .map_err(|_| crate::infrastructure::sqlite::Error::CorruptReceipt)?,
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
    let rows = db.execute("INSERT INTO receipts SELECT value,?1,'POST',?2,?3,?4,?5 FROM metadata WHERE key='principal'",params![key,target,request.to_string(),status,response.to_string()]).map_err(crate::infrastructure::sqlite::Error::from)?;
    if rows != 1 {
        return Err(crate::infrastructure::sqlite::Error::StateConflict);
    }
    Ok(())
}

/// Pending -> final, or an unchanged repeat. A different final state is a
/// conflict: the same key must not be completed twice with different results.
pub fn complete_receipt(
    db: &Connection,
    key: &str,
    target: &str,
    request: &Value,
    status: u16,
    response: &Value,
) -> Result<(), crate::infrastructure::sqlite::Error> {
    let principal = principal(db)?;
    let body = request.to_string();
    let result = response.to_string();
    let existing: Option<(String, String, i64, String)> = db
        .query_row(
            "SELECT method,target,status,response FROM receipts WHERE principal=?1 AND key=?2",
            params![principal, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    let Some((method, old_target, old_status, old_response)) = existing else {
        return Err(crate::infrastructure::sqlite::Error::StateConflict);
    };
    if method != "POST" || old_target != target {
        return Err(crate::infrastructure::sqlite::Error::IdempotencyConflict);
    }
    if old_status != 0 {
        let same = u16::try_from(old_status).ok() == Some(status)
            && serde_json::from_str::<Value>(&old_response)
                .map_err(crate::infrastructure::sqlite::Error::from)?
                == *response;
        return if same {
            Ok(())
        } else {
            Err(crate::infrastructure::sqlite::Error::IdempotencyConflict)
        };
    }
    let rows = db
        .execute(
            "UPDATE receipts SET status=?1,response=?2 WHERE principal=?3 AND key=?4 AND target=?5 AND method='POST' AND status=0 AND request=?6",
            params![status, result, principal, key, target, body],
        )
        .map_err(crate::infrastructure::sqlite::Error::from)?;
    if rows != 1 {
        return Err(crate::infrastructure::sqlite::Error::StateConflict);
    }
    Ok(())
}

/// Drops a pending receipt whose operation had no effect. A completed
/// receipt is never removed here.
pub fn release_receipt(
    db: &Connection,
    key: &str,
    target: &str,
    request: &Value,
) -> Result<(), crate::infrastructure::sqlite::Error> {
    let principal = principal(db)?;
    db.execute(
        "DELETE FROM receipts WHERE principal=?1 AND key=?2 AND target=?3 AND method='POST' AND status=0 AND request=?4",
        params![principal, key, target, request.to_string()],
    )
    .map_err(crate::infrastructure::sqlite::Error::from)?;
    Ok(())
}
