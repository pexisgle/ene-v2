use super::*;

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
