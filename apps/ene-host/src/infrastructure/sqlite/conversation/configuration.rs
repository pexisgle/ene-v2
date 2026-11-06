use super::*;

/// The configuration that is not a companion row, stored and read as one
/// record. Missing fields are the empty values an older host wrote.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Extras {
    grants: Vec<crate::domain::Grant>,
    credential_refs: std::collections::BTreeMap<String, String>,
}

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
    let extras: Extras = extras
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(crate::infrastructure::sqlite::Error::from)?
        .unwrap_or_default();
    Ok(Configuration {
        companions,
        settings,
        grants: extras.grants,
        credential_refs: extras.credential_refs,
    })
}

pub fn write_configuration(
    db: &Connection,
    c: &Configuration,
) -> Result<(), crate::infrastructure::sqlite::Error> {
    for companion in c.companions.values() {
        db.execute("INSERT INTO companions VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![companion.id, serde_json::to_string(companion)?])?;
    }
    let extras = Extras {
        grants: c.grants.clone(),
        credential_refs: c.credential_refs.clone(),
    };
    for (key, value) in [
        ("settings", serde_json::to_string(&c.settings)?),
        ("configuration_extras", serde_json::to_string(&extras)?),
    ] {
        db.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value]).map_err(crate::infrastructure::sqlite::Error::from)?;
    }
    Ok(())
}
