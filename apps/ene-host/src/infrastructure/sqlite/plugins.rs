use super::{OptionalExtension, Result, Store, params};
use serde_json::Value;
impl Store {
    pub async fn plugin_get(&self, namespace: String, key: String) -> Result<Value> {
        self.call(move |db| {
            let value: Option<String> = db
                .query_row(
                    "SELECT value FROM plugin_records WHERE namespace=?1 AND key=?2",
                    params![namespace, key],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .unwrap_or(Value::Null))
        })
        .await
    }
    pub async fn plugin_set(&self, namespace: String, key: String, value: Value) -> Result<()> {
        let value = serde_json::to_string(&value)?;
        self.call(move |db| {
            if namespace.starts_with("state:") {
                let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM plugin_records WHERE namespace=?1 AND key=?2)",params![namespace,key],|row|row.get(0))?;
                let count:i64=db.query_row("SELECT COUNT(*) FROM plugin_records WHERE namespace=?1",[&namespace],|row|row.get(0))?;
                if value.len()>65536 || !exists && count>=256 {return Err(super::Error::StateConflict);}
            }
            db.execute("INSERT INTO plugin_records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value", params![namespace,key,value])?;
            Ok(())
        }).await
    }
    pub async fn plugin_list(&self, namespace: String) -> Result<Vec<Value>> {
        self.call(move |db| {
            let mut query =
                db.prepare("SELECT value FROM plugin_records WHERE namespace=?1 ORDER BY key")?;
            let values = query
                .query_map([namespace], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(values
                .iter()
                .map(|value| serde_json::from_str(value))
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }
}
