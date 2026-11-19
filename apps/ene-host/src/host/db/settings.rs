//! Settings and connections.
use super::{
    Result, Store, corrupt, enum_name_column, json_column, now, optional_json_column,
    require_found, to_json,
};
use crate::host::error::HostError;
use crate::host::mind::Companion;
use crate::host::model::{Credentials, agent_platform, delete_secret};
use crate::host::settings::{Check, Connection, ConnectionConfig, Settings};
use rusqlite::{Row, params};
use std::collections::HashSet;

fn row_to_connection(row: &Row<'_>) -> rusqlite::Result<Connection> {
    let kind = enum_name_column(row, "kind", "unknown connection kind")?;
    let config: ConnectionConfig =
        json_column(row, "config", "stored connection config is corrupt")?;
    if config.kind() != kind {
        return Err(corrupt(
            row.as_ref().column_index("config")?,
            "stored connection kind and config disagree",
        ));
    }
    Ok(Connection {
        id: row.get("id")?,
        name: row.get("name")?,
        enabled: row.get("enabled")?,
        kind,
        config,
        credential_state: crate::host::settings::CredentialState::parse(
            &row.get::<_, String>("credential_state")?,
        ),
        revision: row.get::<_, u64>("revision")?,
        last_check: optional_json_column(row, "last_check", "stored connection check is corrupt")?,
    })
}

const COLUMNS: &str = "id, name, enabled, kind, config, credential_state, last_check, revision";

impl Store {
    pub(crate) fn settings(&self) -> Result<Settings> {
        let (revision, updated_at) = match self.fetch(
            "SELECT revision, updated_at FROM settings_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )? {
            Some(row) => row,
            None => {
                let stamp = now();
                self.conn()?
                    .prepare_cached(
                        "INSERT INTO settings_state (id, revision, updated_at) VALUES (1, 1, ?1)",
                    )?
                    .execute([&stamp])?;
                (1, stamp)
            }
        };
        let connections = self.fetch_all(
            &format!("SELECT {COLUMNS} FROM connections ORDER BY id"),
            [],
            row_to_connection,
        )?;
        Ok(Settings {
            revision,
            connections,
            updated_at,
            policy_texts: self.policy_texts()?,
        })
    }

    pub(crate) fn settings_revision(&self) -> Result<u64> {
        Ok(self
            .fetch(
                "SELECT revision FROM settings_state WHERE id = 1",
                [],
                |row| row.get(0),
            )?
            .unwrap_or(1))
    }

    pub(crate) fn connection(&self, id: &str) -> Result<Option<Connection>> {
        self.fetch(
            &format!("SELECT {COLUMNS} FROM connections WHERE id = ?1"),
            [id],
            row_to_connection,
        )
    }

    /// Applies a settings batch. The expected settings revision must match,
    /// and every connection keeps its own revision for concurrent edits.
    pub(crate) fn update_settings(
        &self,
        expected_revision: u64,
        inputs: &[(Connection, u64)],
    ) -> Result<Settings> {
        let current = self.settings()?;
        if current.revision != expected_revision {
            return Err(HostError::Conflict(
                ("settings changed since they were read").into(),
            ));
        }
        let mut ids = HashSet::new();
        for (input, _) in inputs {
            input.validate()?;
            if !ids.insert(input.id.as_str()) {
                return Err(HostError::Invalid(("connection is listed twice").into()));
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
                Some(existing) if *expected == existing.revision => existing.revision + 1,
                Some(_) => {
                    return Err(HostError::Conflict(format!(
                        "connection `{}` changed since it was read",
                        input.id
                    )));
                }
                None if *expected == 0 => 1,
                None => {
                    return Err(HostError::Conflict(format!(
                        "connection `{}` does not exist",
                        input.id
                    )));
                }
            };
            let reset_check = existing.is_some_and(|previous| {
                input.endpoint().adapter == agent_platform::ADAPTER
                    && input.config != previous.config
            });
            let changed = transaction
                .prepare_cached(
                    "INSERT INTO connections (id, name, enabled, kind, config, credential_state, \
                 last_check, revision, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, \
                 kind = excluded.kind, config = excluded.config, revision = excluded.revision, \
                 updated_at = excluded.updated_at, \
                 last_check = CASE WHEN ?9 THEN NULL ELSE connections.last_check END \
                 WHERE connections.revision = excluded.revision - 1",
                )?
                .execute(params![
                    input.id,
                    input.name,
                    input.enabled,
                    input.kind.as_str(),
                    to_json(&input.config)?,
                    input.credential_state.as_str(),
                    revision,
                    stamp,
                    reset_check,
                ])?;
            if changed == 0 {
                return Err(HostError::Conflict(format!(
                    "connection `{}` changed since it was read",
                    input.id
                )));
            }
        }
        let changed = transaction
            .prepare_cached(
                "UPDATE settings_state SET revision = ?2, updated_at = ?3 \
             WHERE id = 1 AND revision = ?4",
            )?
            .execute(params![1, next, stamp, expected_revision])?;
        if changed == 0 {
            return Err(HostError::Conflict(
                ("settings changed since they were read").into(),
            ));
        }
        transaction.commit()?;
        drop(connection);
        self.settings()
    }

    pub(crate) fn set_credential_state(
        &self,
        id: &str,
        state: crate::host::settings::CredentialState,
    ) -> Result<()> {
        let changed = self
            .conn()?
            .prepare_cached(
                "UPDATE connections SET credential_state = ?2, updated_at = ?3 WHERE id = ?1",
            )?
            .execute(params![id, state.as_str(), now()])?;
        require_found(changed, "connection not found")
    }

    pub(crate) fn delete_connection(
        &self,
        id: &str,
        revision: u64,
        credentials: &Credentials,
    ) -> Result<(Settings, Vec<Companion>)> {
        let mut companions = self.companions()?;
        let mut db = self.conn()?;
        let tx = db.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM connections WHERE id = ?1 AND revision = ?2",
            params![id, revision],
        )?;
        if deleted == 0 {
            return Err(HostError::Conflict(
                "connection changed or was already deleted".into(),
            ));
        }
        let stamp = now();
        companions.retain_mut(|companion| companion.assignments.remove_connection(id));
        for companion in &mut companions {
            companion.revision += 1;
            companion.updated_at.clone_from(&stamp);
            tx.execute(
                "UPDATE companions SET assignments = ?1, revision = ?2, updated_at = ?3 WHERE id = ?4",
                params![to_json(&companion.assignments)?, companion.revision, stamp, companion.id],
            )?;
        }
        tx.execute(
            "UPDATE settings_state SET revision = revision + 1, updated_at = ?1 WHERE id = 1",
            [&stamp],
        )?;
        // Do not commit the removal if the registered key could not be erased.
        delete_secret(credentials, id)?;
        tx.commit()?;
        drop(db);
        Ok((self.settings()?, companions))
    }

    pub(crate) fn set_check(&self, id: &str, check: &Check) -> Result<()> {
        let changed = self
            .conn()?
            .prepare_cached(
                "UPDATE connections SET last_check = ?2, updated_at = ?3 WHERE id = ?1",
            )?
            .execute(params![id, to_json(check)?, now()])?;
        require_found(changed, "connection not found")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::settings::{
        ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
    };

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
    fn historical_unknown_credential_state_is_absent_but_invalid_kinds_are_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        store.update_settings(1, &[(connection("c1"), 0)]).unwrap();
        for (stored, expected) in [
            ("absent", CredentialState::Absent),
            ("registered", CredentialState::Registered),
            ("environment", CredentialState::Environment),
            ("historical-unknown", CredentialState::Absent),
        ] {
            store
                .conn()
                .unwrap()
                .execute(
                    "UPDATE connections SET credential_state = ?1 WHERE id = 'c1'",
                    [stored],
                )
                .unwrap();
            assert_eq!(
                store.connection("c1").unwrap().unwrap().credential_state,
                expected
            );
        }
        for kind in ["unknown", "", "Llm", " llm", "tts"] {
            store
                .conn()
                .unwrap()
                .execute("UPDATE connections SET kind = ?1 WHERE id = 'c1'", [kind])
                .unwrap();
            assert!(
                matches!(store.connection("c1"), Err(HostError::Storage(_))),
                "{kind}"
            );
        }
        store
            .conn()
            .unwrap()
            .execute("UPDATE connections SET kind = 'llm' WHERE id = 'c1'", [])
            .unwrap();
        assert_eq!(
            store.connection("c1").unwrap().unwrap().kind,
            ConnectionKind::Llm
        );
    }

    #[test]
    fn repeated_settings_mutations_keep_bindings_revisions_and_missing_rows_separate() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        assert_eq!(store.settings().unwrap().revision, 1);
        let mut settings = store
            .update_settings(1, &[(connection("c1"), 0), (connection("c2"), 0)])
            .unwrap();
        for (id, state, healthy) in [
            ("c1", CredentialState::Registered, true),
            ("c2", CredentialState::Environment, false),
            ("c1", CredentialState::Absent, false),
        ] {
            let check = Check {
                healthy,
                checked_at: now(),
                ..Default::default()
            };
            store.set_credential_state(id, state).unwrap();
            store.set_check(id, &check).unwrap();
            let stored = store.connection(id).unwrap().unwrap();
            assert_eq!(stored.credential_state, state);
            assert_eq!(stored.last_check, Some(check));
        }
        assert!(matches!(
            store.set_credential_state("missing", CredentialState::Absent),
            Err(HostError::NotFound(_))
        ));
        assert!(matches!(
            store.set_check("missing", &Check::default()),
            Err(HostError::NotFound(_))
        ));
        for id in ["c2", "c1", "c2"] {
            let mut changed = store.connection(id).unwrap().unwrap();
            let expected = changed.revision;
            changed.name.push_str(" edited");
            settings = store
                .update_settings(settings.revision, &[(changed.clone(), expected)])
                .unwrap();
            let stored = store.connection(id).unwrap().unwrap();
            assert_eq!(stored.name, changed.name);
            assert_eq!(stored.revision, expected + 1);
        }
    }

    #[test]
    fn removing_a_connection_checks_revision_and_clears_only_its_bindings_and_secret() {
        use crate::host::mind::{Assignment, default_ene_companion};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        let settings = store
            .update_settings(1, &[(connection("drop"), 0), (connection("keep"), 0)])
            .unwrap();
        let credentials = Credentials::new(true);
        credentials.remember("drop", "discarded".into());
        credentials.remember("keep", "preserved".into());
        let mut companion = default_ene_companion(&now());
        let assignment = |id: &str| Assignment {
            connection_id: id.into(),
            model_id: "gpt-5".into(),
        };
        companion.assignments.companion = assignment("drop");
        companion.assignments.task = assignment("keep");
        companion.assignments.perception = Some(assignment("drop"));
        store.insert_companion(&companion).unwrap();

        assert!(matches!(
            store.delete_connection("drop", 2, &credentials),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(
            credentials.secret("drop").unwrap().as_deref(),
            Some("discarded")
        );
        assert_eq!(store.companion(&companion.id).unwrap().unwrap(), companion);
        let (next, changed) = store.delete_connection("drop", 1, &credentials).unwrap();
        assert_eq!(next.revision, settings.revision + 1);
        assert_eq!(
            next.connections
                .iter()
                .map(|conn| conn.id.as_str())
                .collect::<Vec<_>>(),
            ["keep"]
        );
        assert_eq!(changed.len(), 1);
        let saved = store.companion(&companion.id).unwrap().unwrap();
        assert_eq!(saved, changed[0]);
        assert!(!saved.assignments.companion.is_set());
        assert_eq!(saved.assignments.task, assignment("keep"));
        assert!(saved.assignments.perception.is_none());
        assert_eq!(saved.revision, companion.revision + 1);
        assert!(credentials.secret("drop").unwrap().is_none());
        assert_eq!(
            credentials.secret("keep").unwrap().as_deref(),
            Some("preserved")
        );
    }

    #[test]
    fn a_stale_connection_rolls_back_the_whole_settings_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let before = store
            .update_settings(1, &[(connection("c1"), 0), (connection("c2"), 0)])
            .unwrap();
        let mut changed = connection("c1");
        changed.name = "updated".into();
        assert!(matches!(
            store.update_settings(before.revision, &[(changed, 1), (connection("c2"), 0)]),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(store.settings().unwrap(), before);
        assert_eq!(store.settings_revision().unwrap(), before.revision);
    }
}
