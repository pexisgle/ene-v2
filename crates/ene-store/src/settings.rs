//! Settings and connections persistence.

use crate::row::FromRow;
use crate::{
    Store, StoreError, corrupt, enum_name_column, error::Result, id_column, json_column,
    optional_json_column, require_found, to_json, util::now,
};
use ene_domain::{
    Check, Connection, ConnectionConfig, ConnectionId, ConnectionKind, CredentialState, Settings,
};
use ene_mind::Companion;
use rusqlite::{Row, params};
use std::collections::HashSet;

impl FromRow for Connection {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
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
            id: id_column(row, "id", "stored connection id is invalid")?,
            name: row.get("name")?,
            enabled: row.get("enabled")?,
            kind,
            config,
            credential_state: CredentialState::parse(&row.get::<_, String>("credential_state")?),
            revision: row.get::<_, u64>("revision")?,
            last_check: optional_json_column(
                row,
                "last_check",
                "stored connection check is corrupt",
            )?,
        })
    }
}

fn connection_with_creation(row: &Row<'_>) -> rusqlite::Result<(Connection, u64)> {
    Ok((Connection::from_row(row)?, row.get("created_revision")?))
}

const COLUMNS: &str = "id, name, enabled, kind, config, credential_state, last_check, revision";

/// Repository managing `settings_state` and `connections`.
#[derive(Clone, Copy, Debug)]
pub struct SettingsRepo<'a>(&'a Store);

fn changes_mcp_credential_receiver(
    current: &ConnectionConfig,
    proposed: &ConnectionConfig,
) -> bool {
    match (current, proposed) {
        (ConnectionConfig::Mcp(current), ConnectionConfig::Mcp(proposed)) => {
            !current.same_credential_receiver(proposed)
        }
        (ConnectionConfig::Mcp(_), _) | (_, ConnectionConfig::Mcp(_)) => true,
        _ => false,
    }
}

impl<'a> SettingsRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn settings(&self) -> Result<Settings> {
        let (revision, updated_at) = match self.0.fetch(
            "SELECT revision, updated_at FROM settings_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )? {
            Some(row) => row,
            None => {
                let stamp = now();
                self.0
                    .conn()
                    .prepare_cached(
                        "INSERT INTO settings_state (id, revision, updated_at) VALUES (1, 1, ?1)",
                    )?
                    .execute([&stamp])?;
                (1, stamp)
            }
        };
        let connections = self.0.fetch_all(
            &format!("SELECT {COLUMNS} FROM connections ORDER BY id"),
            [],
            Connection::from_row,
        )?;
        Ok(Settings {
            revision,
            connections,
            updated_at,
            policy_texts: self.0.policy_repo().policy_texts()?,
        })
    }

    pub fn settings_revision(&self) -> Result<u64> {
        Ok(self
            .0
            .fetch(
                "SELECT revision FROM settings_state WHERE id = 1",
                [],
                |row| row.get(0),
            )?
            .unwrap_or(1))
    }

    pub fn connection(&self, id: &str) -> Result<Option<Connection>> {
        self.0.fetch(
            &format!("SELECT {COLUMNS} FROM connections WHERE id = ?1"),
            [id],
            Connection::from_row,
        )
    }

    /// Reads a connection with the settings revision at which its ID was created.
    pub fn connection_with_creation(&self, id: &str) -> Result<Option<(Connection, u64)>> {
        self.0.fetch(
            &format!("SELECT {COLUMNS}, created_revision FROM connections WHERE id = ?1"),
            [id],
            connection_with_creation,
        )
    }

    pub fn enabled_mcp_connections(&self) -> Result<Vec<(Connection, u64)>> {
        self.0.fetch_all(
            &format!(
                "SELECT {COLUMNS}, created_revision FROM connections \
                WHERE kind = 'mcp' AND enabled ORDER BY id"
            ),
            [],
            connection_with_creation,
        )
    }

    /// Enabled search connections in selection order, with their original settings revision.
    /// The revision survives edits and differs when the same ID is deleted and created again.
    pub fn enabled_search_connections(&self) -> Result<Vec<(Connection, u64)>> {
        self.0.fetch_all(
            &format!(
                "SELECT {COLUMNS}, created_revision FROM connections \
                WHERE kind = 'search' AND enabled ORDER BY id LIMIT 2"
            ),
            [],
            connection_with_creation,
        )
    }

    /// Applies a settings batch. The expected settings revision must match,
    /// and every connection keeps its own revision for concurrent edits.
    pub fn update_settings(
        &self,
        expected_revision: u64,
        inputs: &[(Connection, u64)],
    ) -> Result<Settings> {
        let current = self.settings()?;
        if current.revision != expected_revision {
            return Err(StoreError::Conflict(
                "settings changed since they were read".into(),
            ));
        }
        let mut ids = HashSet::new();
        for (input, _) in inputs {
            input.validate()?;
            if !ids.insert(&input.id) {
                return Err(StoreError::Invalid("connection is listed twice".into()));
            }
        }
        let stamp = now();
        let next = expected_revision + 1;
        self.0.write_tx(|transaction| {
            for (input, expected) in inputs {
                let existing = current
                    .connections
                    .iter()
                    .find(|existing| existing.id == input.id);
                let revision = match existing {
                    Some(existing) if *expected == existing.revision => existing.revision + 1,
                    Some(_) => {
                        return Err(StoreError::Conflict(format!(
                            "connection `{}` changed since it was read",
                            input.id
                        )));
                    }
                    // The settings epoch is above every previous row version,
                    // including versions of a connection with this same ID.
                    None if *expected == 0 => next,
                    None => {
                        return Err(StoreError::Conflict(format!(
                            "connection `{}` does not exist",
                            input.id
                        )));
                    }
                };
                if existing.is_some_and(|previous| {
                    previous.credential_state == CredentialState::Registered
                        && changes_mcp_credential_receiver(&previous.config, &input.config)
                }) {
                    return Err(StoreError::Conflict(
                        "remove the registered credential before changing the MCP receiver".into(),
                    ));
                }
                let reset_check = existing.is_some_and(|previous| {
                    input.kind != previous.kind
                        || input.endpoint().adapter != previous.endpoint().adapter
                        || input.endpoint().endpoint != previous.endpoint().endpoint
                        || (matches!(input.kind, ConnectionKind::Mcp | ConnectionKind::Acp)
                            && (input.config != previous.config
                                || input.enabled != previous.enabled))
                        || (input.kind == ConnectionKind::Search && input.config != previous.config)
                        || (input.endpoint().adapter == ene_catalog::GOOGLE_AGENT_PLATFORM
                            && input.config != previous.config
                            && !keeps_verified_model_list(previous, input))
                });
                let changed = transaction
                .prepare_cached(
                    "INSERT INTO connections (id, name, enabled, kind, config, credential_state, \
                        last_check, revision, updated_at, created_revision) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?10) \
                     ON CONFLICT(id) DO UPDATE SET \
                        name = excluded.name, enabled = excluded.enabled, kind = excluded.kind, \
                        config = excluded.config, revision = excluded.revision, \
                        updated_at = excluded.updated_at, \
                        last_check = CASE WHEN ?9 THEN NULL ELSE connections.last_check END \
                     WHERE connections.revision = ?11",
                )?
                .execute(params![
                    input.id.as_str(),
                    input.name,
                    input.enabled,
                    input.kind.as_str(),
                    to_json(&input.config)?,
                    input.credential_state.as_str(),
                    revision,
                    stamp,
                    reset_check,
                    next,
                    expected,
                ])?;
                if changed == 0 {
                    return Err(StoreError::Conflict(format!(
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
                return Err(StoreError::Conflict(
                    "settings changed since they were read".into(),
                ));
            }
            Ok(())
        })?;
        self.settings()
    }

    pub fn set_credential_state(&self, id: &ConnectionId, state: CredentialState) -> Result<()> {
        let changed = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE connections SET credential_state = ?2, updated_at = ?3,
                 last_check = NULL WHERE id = ?1",
            )?
            .execute(params![id.as_str(), state.as_str(), now()])?;
        require_found(changed, "connection not found")
    }

    pub fn delete_connection<E>(
        &self,
        id: &ConnectionId,
        revision: u64,
        on_delete_secret: impl FnOnce() -> std::result::Result<(), E>,
    ) -> std::result::Result<(Settings, Vec<Companion>), E>
    where
        E: From<StoreError>,
    {
        let mut companions = self.0.companion_repo().companions()?;
        let mut db = self.0.conn();
        let tx = db.transaction().map_err(StoreError::from)?;
        let deleted = tx
            .execute(
                "DELETE FROM connections WHERE id = ?1 AND revision = ?2",
                params![id.as_str(), revision],
            )
            .map_err(StoreError::from)?;
        if deleted == 0 {
            return Err(E::from(StoreError::Conflict(
                "connection changed or was already deleted".into(),
            )));
        }
        let stamp = now();
        companions.retain_mut(|companion| companion.assignments.remove_connection(id.as_str()));
        for companion in &mut companions {
            companion.revision += 1;
            companion.updated_at.clone_from(&stamp);
            let encoded = to_json(&companion.assignments)?;
            tx.execute(
                "UPDATE companions SET assignments = ?1, revision = ?2, updated_at = ?3 WHERE id = ?4",
                params![encoded, companion.revision, stamp, companion.id],
            )
            .map_err(StoreError::from)?;
        }
        tx.execute(
            "UPDATE settings_state SET revision = revision + 1, updated_at = ?1 WHERE id = 1",
            [&stamp],
        )
        .map_err(StoreError::from)?;
        // Do not commit the removal if the registered key could not be erased.
        on_delete_secret()?;
        tx.commit().map_err(StoreError::from)?;
        drop(db);
        Ok((self.settings()?, companions))
    }

    /// Saves a probe only for the checked incarnation and revision.
    pub fn set_check(
        &self,
        id: &ConnectionId,
        revision: u64,
        created_revision: u64,
        check: &Check,
    ) -> Result<()> {
        let changed = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE connections SET last_check = ?3, updated_at = ?4
                 WHERE id = ?1 AND revision = ?2 AND created_revision = ?5",
            )?
            .execute(params![
                id.as_str(),
                revision,
                to_json(check)?,
                now(),
                created_revision
            ])?;
        if changed == 0 {
            return Err(StoreError::Conflict(
                "connection changed during the check".into(),
            ));
        }
        Ok(())
    }
}

fn keeps_verified_model_list(previous: &Connection, next: &Connection) -> bool {
    let ConnectionConfig::Llm(config) = &next.config else {
        return false;
    };
    previous.endpoint() == next.endpoint()
        && previous.last_check.as_ref().is_some_and(|check| {
            check.healthy
                && check
                    .capabilities
                    .iter()
                    .any(|cap| cap == ene_catalog::GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST)
                && config
                    .model_ids
                    .iter()
                    .all(|id| check.resources.iter().any(|known| known == id.trim()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_domain::{Endpoint, LlmConnection};

    fn connection(id: &str) -> Connection {
        Connection {
            id: ConnectionId::parse(id).unwrap(),
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
    fn legacy_credential_states_are_tolerated_and_invalid_connections_fail_to_load() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.settings_repo();
        repo.update_settings(1, &[(connection("c1"), 0)]).unwrap();
        for (stored, expected) in [
            ("absent", CredentialState::Absent),
            ("registered", CredentialState::Registered),
            ("environment", CredentialState::Environment),
            ("historical-unknown", CredentialState::Absent),
        ] {
            store
                .conn()
                .execute(
                    "UPDATE connections SET credential_state = ?1 WHERE id = 'c1'",
                    [stored],
                )
                .unwrap();
            assert_eq!(
                repo.connection("c1").unwrap().unwrap().credential_state,
                expected
            );
        }
        for invalid_kind in ["unknown", ""] {
            store
                .conn()
                .execute(
                    "UPDATE connections SET kind = ?1 WHERE id = 'c1'",
                    [invalid_kind],
                )
                .unwrap();
            assert!(matches!(repo.connection("c1"), Err(StoreError::Storage(_))));
        }
        store
            .conn()
            .execute("UPDATE connections SET kind = 'tts' WHERE id = 'c1'", [])
            .unwrap();
        assert!(matches!(repo.connection("c1"), Err(StoreError::Storage(_))));
        store
            .conn()
            .execute(
                "UPDATE connections SET kind = 'llm', id = 'bad id' WHERE id = 'c1'",
                [],
            )
            .unwrap();
        assert!(matches!(
            repo.connection("bad id"),
            Err(StoreError::Storage(_))
        ));
    }

    #[test]
    fn a_probe_is_invalidated_when_its_llm_receiver_or_credential_changes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.settings_repo();
        let settings = repo.update_settings(1, &[(connection("c1"), 0)]).unwrap();
        let (mut active, created) = repo.connection_with_creation("c1").unwrap().unwrap();
        let check = Check {
            healthy: true,
            ..Default::default()
        };
        repo.set_check(&active.id, active.revision, created, &check)
            .unwrap();
        let ConnectionConfig::Llm(config) = &mut active.config else {
            panic!("LLM fixture")
        };
        config.endpoint.endpoint = "http://127.0.0.1:8142/v1".into();
        let id = active.id.clone();
        let revision = active.revision;
        repo.update_settings(settings.revision, &[(active, revision)])
            .unwrap();
        let active = repo.connection(id.as_str()).unwrap().unwrap();
        assert!(active.last_check.is_none());
        repo.set_check(&id, active.revision, created, &check)
            .unwrap();
        repo.set_credential_state(&id, CredentialState::Registered)
            .unwrap();
        assert!(
            repo.connection(id.as_str())
                .unwrap()
                .unwrap()
                .last_check
                .is_none()
        );
    }

    #[test]
    fn updating_settings_checks_revisions_and_keeps_per_connection_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.settings_repo();
        let mut settings = repo
            .update_settings(1, &[(connection("c1"), 0), (connection("c2"), 0)])
            .unwrap();
        assert_eq!(settings.revision, 2);
        assert!(matches!(
            repo.set_credential_state(
                &ConnectionId::parse("missing").unwrap(),
                CredentialState::Absent
            ),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            repo.set_check(
                &ConnectionId::parse("missing").unwrap(),
                1,
                2,
                &Check::default()
            ),
            Err(StoreError::Conflict(_))
        ));
        for id in ["c2", "c1", "c2"] {
            let mut changed = repo.connection(id).unwrap().unwrap();
            let expected = changed.revision;
            changed.name.push_str(" edited");
            settings = repo
                .update_settings(settings.revision, &[(changed.clone(), expected)])
                .unwrap();
            let stored = repo.connection(id).unwrap().unwrap();
            assert_eq!(stored.name, changed.name);
            assert_eq!(stored.revision, expected + 1);
            assert!(matches!(
                repo.set_check(&stored.id, expected, 2, &Check::default()),
                Err(StoreError::Conflict(_))
            ));
            assert!(repo.connection(id).unwrap().unwrap().last_check.is_none());
        }
        let check = Check {
            healthy: true,
            ..Default::default()
        };
        let current = repo.connection("c1").unwrap().unwrap();
        repo.set_check(&current.id, current.revision, 2, &check)
            .unwrap();
        assert_eq!(
            repo.connection("c1").unwrap().unwrap().last_check,
            Some(check)
        );
        let (settings, _) = repo
            .delete_connection(&current.id, current.revision, || Ok::<_, StoreError>(()))
            .unwrap();
        repo.update_settings(settings.revision, &[(connection("c1"), 0)])
            .unwrap();
        let (replacement, created) = repo.connection_with_creation("c1").unwrap().unwrap();
        assert_ne!(created, 2);
        assert!(replacement.revision > current.revision);
        assert!(matches!(
            repo.delete_connection::<StoreError>(&current.id, current.revision, || {
                panic!("stale deletion cannot reach credential erasure")
            }),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(repo.connection("c1").unwrap().as_ref(), Some(&replacement));
        assert!(matches!(
            repo.set_check(&replacement.id, replacement.revision, 2, &Check::default()),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn removing_a_connection_checks_revision_and_clears_only_its_bindings_and_secret() {
        use ene_mind::{ModelAssignment, default_ene_companion};
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        let repo = store.settings_repo();
        let companions = store.companion_repo();
        let settings = repo
            .update_settings(1, &[(connection("drop"), 0), (connection("keep"), 0)])
            .unwrap();
        let revision = repo.connection("drop").unwrap().unwrap().revision;
        let mut secrets = HashMap::from([
            ("drop".to_owned(), "discarded".to_owned()),
            ("keep".to_owned(), "preserved".to_owned()),
        ]);
        let mut companion = default_ene_companion(&now());
        let assignment = |id: &str| ModelAssignment {
            connection_id: id.into(),
            model_id: "gpt-5".into(),
        };
        companion.assignments.companion = assignment("drop");
        companion.assignments.task = assignment("keep");
        companion.assignments.perception = Some(assignment("drop"));
        companions.insert_companion(&companion).unwrap();

        assert!(matches!(
            repo.delete_connection(
                &ConnectionId::parse("drop").unwrap(),
                revision - 1,
                || -> Result<()> {
                    secrets.remove("drop");
                    Ok(())
                }
            ),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(secrets.get("drop").map(String::as_str), Some("discarded"));
        assert_eq!(
            companions.companion(&companion.id).unwrap().unwrap(),
            companion
        );

        // If secret erasure fails, the SQLite transaction must roll back cleanly.
        assert!(matches!(
            repo.delete_connection(
                &ConnectionId::parse("drop").unwrap(),
                revision,
                || -> Result<()> { Err(StoreError::Storage("keyring unavailable".into())) }
            ),
            Err(StoreError::Storage(_))
        ));
        assert!(repo.connection("drop").unwrap().is_some());
        assert_eq!(
            companions.companion(&companion.id).unwrap().unwrap(),
            companion
        );

        let (next, changed) = repo
            .delete_connection(
                &ConnectionId::parse("drop").unwrap(),
                revision,
                || -> Result<()> {
                    secrets.remove("drop");
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(next.revision, settings.revision + 1);
        assert_eq!(
            next.connections
                .iter()
                .map(|conn| conn.id.as_str())
                .collect::<Vec<_>>(),
            ["keep"]
        );
        assert_eq!(changed.len(), 1);
        let saved = companions.companion(&companion.id).unwrap().unwrap();
        assert_eq!(saved, changed[0]);
        assert!(!saved.assignments.companion.is_set());
        assert_eq!(saved.assignments.task, assignment("keep"));
        assert!(saved.assignments.perception.is_none());
        assert_eq!(saved.revision, companion.revision + 1);
        assert!(!secrets.contains_key("drop"));
        assert_eq!(secrets.get("keep").map(String::as_str), Some("preserved"));
    }

    #[test]
    fn a_stale_connection_rolls_back_the_whole_settings_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.settings_repo();
        let before = repo
            .update_settings(1, &[(connection("c1"), 0), (connection("c2"), 0)])
            .unwrap();
        let mut changed = connection("c1");
        changed.name = "updated".into();
        assert!(matches!(
            repo.update_settings(
                before.revision,
                &[
                    (changed, before.connections[0].revision),
                    (connection("c2"), 0)
                ]
            ),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(repo.settings().unwrap(), before);
        assert_eq!(repo.settings_revision().unwrap(), before.revision);
    }
}
