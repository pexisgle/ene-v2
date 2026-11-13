//! Settings and connections.
use super::{Result, Store, now, to_json};
use crate::host::error::HostError;
use crate::host::settings::{Check, Connection, ConnectionConfig, ConnectionKind, Settings};
use rusqlite::{OptionalExtension, Row, params};
use std::collections::HashSet;

fn row_to_connection(row: &Row<'_>) -> rusqlite::Result<Connection> {
    let kind: String = row.get("kind")?;
    let kind = ConnectionKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            kind.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("unknown connection kind")),
        )
    })?;
    let config: String = row.get("config")?;
    let config: ConnectionConfig = serde_json::from_str(&config).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            config.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("stored connection config is corrupt")),
        )
    })?;
    if config.kind() != kind {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "stored connection kind and config disagree",
            )),
        ));
    }
    let last_check: Option<String> = row.get("last_check")?;
    let credential_state: String = row.get("credential_state")?;
    Ok(Connection {
        id: row.get("id")?,
        name: row.get("name")?,
        enabled: row.get("enabled")?,
        kind,
        config,
        credential_state: crate::host::settings::CredentialState::parse(&credential_state),
        revision: row.get::<_, u64>("revision")?,
        last_check: last_check
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other("stored connection check is corrupt")),
                )
            })?,
    })
}

const COLUMNS: &str = "id, name, enabled, kind, config, credential_state, last_check, revision";

impl Store {
    pub(crate) fn settings(&self) -> Result<Settings> {
        let connection = self.conn()?;
        let row: Option<(u64, String)> = connection
            .query_row(
                "SELECT revision, updated_at FROM settings_state WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (revision, updated_at) = match row {
            Some(row) => row,
            None => {
                let stamp = now();
                connection.execute(
                    "INSERT INTO settings_state (id, revision, updated_at) VALUES (1, 1, ?1)",
                    [&stamp],
                )?;
                (1, stamp)
            }
        };
        let mut statement =
            connection.prepare(&format!("SELECT {COLUMNS} FROM connections ORDER BY id"))?;
        let connections = statement
            .query_map([], row_to_connection)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Settings {
            revision,
            connections,
            updated_at,
        })
    }

    pub(crate) fn settings_revision(&self) -> Result<u64> {
        Ok(self.settings()?.revision)
    }

    pub(crate) fn connection(&self, id: &str) -> Result<Option<Connection>> {
        let connection = self.conn()?;
        let connection = connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM connections WHERE id = ?1"),
                [id],
                row_to_connection,
            )
            .optional()?;
        Ok(connection)
    }

    /// Applies a settings batch. The expected settings revision must match,
    /// and every connection keeps its own revision for concurrent edits.
    pub(crate) fn update_settings(
        &self,
        expected_revision: u64,
        inputs: &[(Connection, u64)],
        remove_ids: &[String],
    ) -> Result<Settings> {
        let current = self.settings()?;
        if current.revision != expected_revision {
            return Err(HostError::conflict("settings changed since they were read"));
        }
        let mut ids = HashSet::new();
        for (input, _) in inputs {
            input.validate()?;
            if !ids.insert(input.id.as_str()) {
                return Err(HostError::invalid("connection is listed twice"));
            }
        }
        let stamp = now();
        let next = expected_revision + 1;
        let mut connection = self.conn()?;
        let transaction = connection.transaction()?;
        for (input, expected) in inputs {
            let existing = current
                .connections
                .iter()
                .find(|existing| existing.id == input.id);
            let revision = match existing {
                Some(existing) => {
                    if *expected != existing.revision {
                        return Err(HostError::conflict(format!(
                            "connection `{}` changed since it was read",
                            input.id
                        )));
                    }
                    existing.revision + 1
                }
                None => {
                    if *expected != 0 {
                        return Err(HostError::conflict(format!(
                            "connection `{}` does not exist",
                            input.id
                        )));
                    }
                    1
                }
            };
            transaction.execute(
                "INSERT INTO connections (id, name, enabled, kind, config, credential_state, \
                 last_check, revision, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, \
                 kind = excluded.kind, config = excluded.config, revision = excluded.revision, \
                 updated_at = excluded.updated_at",
                params![
                    input.id,
                    input.name,
                    input.enabled,
                    input.kind.as_str(),
                    to_json(&input.config)?,
                    input.credential_state.as_str(),
                    revision,
                    stamp,
                ],
            )?;
        }
        for id in remove_ids {
            super::validate_id(id)?;
            transaction.execute("DELETE FROM connections WHERE id = ?1", [id])?;
        }
        transaction.execute(
            "UPDATE settings_state SET revision = ?2, updated_at = ?3 WHERE id = 1",
            params![1, next, stamp],
        )?;
        transaction.commit()?;
        drop(connection);
        self.settings()
    }

    pub(crate) fn set_credential_state(
        &self,
        id: &str,
        state: crate::host::settings::CredentialState,
    ) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE connections SET credential_state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now()],
        )?;
        if changed == 0 {
            return Err(HostError::not_found("connection not found"));
        }
        Ok(())
    }

    pub(crate) fn set_check(&self, id: &str, check: &Check) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE connections SET last_check = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to_json(check)?, now()],
        )?;
        if changed == 0 {
            return Err(HostError::not_found("connection not found"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{ConnectionConfig, CredentialState, Endpoint, LlmConnection};

    fn connection(id: &str) -> Connection {
        Connection {
            id: id.into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openai".into(),
                    endpoint: "https://api.openai.com/v1".into(),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_ids: vec!["gpt-5".into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    #[test]
    fn settings_batches_check_revisions_and_kinds() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let settings = store.settings().unwrap();
        assert_eq!(settings.revision, 1);
        let settings = store
            .update_settings(1, &[(connection("c1"), 0)], &[])
            .unwrap();
        assert_eq!((settings.revision, settings.connections.len()), (2, 1));
        assert!(store.update_settings(1, &[], &[]).is_err());
        store
            .set_credential_state("c1", CredentialState::Registered)
            .unwrap();
        assert_eq!(
            store.connection("c1").unwrap().unwrap().credential_state,
            CredentialState::Registered
        );
        let check = Check {
            healthy: true,
            checked_at: now(),
            failure: None,
            capabilities: vec!["streaming_text".into()],
            resources: vec!["gpt-5".into()],
        };
        store.set_check("c1", &check).unwrap();
        assert_eq!(
            store.connection("c1").unwrap().unwrap().last_check,
            Some(check)
        );
        let settings = store.update_settings(2, &[], &["c1".to_owned()]).unwrap();
        assert!(settings.connections.is_empty());
    }
}
