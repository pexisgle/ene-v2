//! One transactional upgrade of persisted extension objects to JsonDocument.
use super::*;
use serde_json::Value;

fn document(value: &mut Value) -> Result<()> {
    *value = serde_json::to_value(ene_rpc::JsonDocument {
        json: value.to_string(),
        ..Default::default()
    })?;
    Ok(())
}
fn fields(value: &mut Value, names: &[&str]) -> Result<()> {
    for name in names {
        if let Some(field) = value.get_mut(*name) {
            document(field)?;
        }
    }
    Ok(())
}
fn failure(value: &mut Value) -> Result<()> {
    if let Some(error) = value.get_mut("failure") {
        fields(error, &["details"])?;
    }
    if let Some(error) = value.get_mut("error") {
        fields(error, &["details"])?;
    }
    Ok(())
}
fn resources(value: &mut Value) -> Result<()> {
    if let Some(items) = value.get_mut("resources").and_then(Value::as_array_mut) {
        for item in items {
            document(item)?;
        }
    }
    Ok(())
}
pub(super) fn migrate(tx: &tokio_rusqlite::rusqlite::Transaction<'_>) -> Result<()> {
    // The seed already constructs generated messages. This transient marker
    // distinguishes fresh databases from old arbitrary JSON, even {"json":"1"}.
    if tx.execute(
        "DELETE FROM metadata WHERE key='generated_messages_seed'",
        [],
    )? > 0
    {
        return Ok(());
    }
    for table in ["companions", "messages"] {
        let rows: Vec<(String, String)> = {
            let mut query = tx.prepare(&format!("SELECT id,body FROM {table}"))?;
            query
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?
        };
        for (id, body) in rows {
            let mut value: Value = serde_json::from_str(&body)?;
            if table == "companions" {
                fields(&mut value, &["body", "voice", "availability"])?;
            } else {
                resources(&mut value)?;
                failure(&mut value)?;
            }
            tx.execute(
                &format!("UPDATE {table} SET body=?2 WHERE id=?1"),
                params![id, value.to_string()],
            )?;
        }
    }
    let mut settings: Value = serde_json::from_str(&super::super::queries::meta(tx, "settings")?)?;
    if let Some(connections) = settings
        .get_mut("connections")
        .and_then(Value::as_array_mut)
    {
        for connection in connections {
            fields(
                connection,
                &["options", "capabilities", "last_check", "lastCheck"],
            )?;
        }
    }
    tx.execute(
        "UPDATE metadata SET value=?1 WHERE key='settings'",
        [settings.to_string()],
    )?;
    let receipts: Vec<(String, String, String, String)> = {
        let mut query = tx.prepare("SELECT principal,key,request,response FROM receipts")?;
        query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (principal, key, request, response) in receipts {
        let mut request: Value = serde_json::from_str(&request)?;
        resources(&mut request)?;
        let mut response: Value = serde_json::from_str(&response)?;
        failure(&mut response)?;
        fields(&mut response, &["capabilities"])?;
        if let Some(grant) = response.get_mut("grant") {
            fields(grant, &["connection_fingerprint", "connectionFingerprint"])?;
        }
        tx.execute(
            "UPDATE receipts SET request=?3,response=?4 WHERE principal=?1 AND key=?2",
            params![principal, key, request.to_string(), response.to_string()],
        )?;
    }
    let bindings: Vec<(String, String)> = {
        let mut query =
            tx.prepare("SELECT key,value FROM plugin_records WHERE namespace='bindings'")?;
        query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (key, body) in bindings {
        let mut value: Value = serde_json::from_str(&body)?;
        fields(&mut value, &["configuration"])?;
        tx.execute(
            "UPDATE plugin_records SET value=?2 WHERE namespace='bindings' AND key=?1",
            params![key, value.to_string()],
        )?;
    }
    Ok(())
}
