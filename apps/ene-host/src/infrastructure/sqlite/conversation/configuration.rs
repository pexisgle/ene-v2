use super::*;
pub fn load(db: &Connection) -> Result<Configuration, crate::infrastructure::sqlite::Error> {
    let mut companions = std::collections::BTreeMap::new();
    let mut query = db.prepare("SELECT id,body FROM companions")?;
    for row in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (id, body) = row?;
        companions.insert(id, serde_json::from_str(&body)?);
    }
    let settings = serde_json::from_str(&super::super::queries::meta(db, "settings")?)?;
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
        companions,
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
    for companion in c.companions.values() {
        db.execute("INSERT INTO companions VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![companion.id, serde_json::to_string(companion)?])?;
    }
    for (key, value) in [
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
