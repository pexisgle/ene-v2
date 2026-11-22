//! SQLite persistence engine, forward migrations, and domain repositories.

pub mod approvals;
pub mod clients;
pub mod companions;
pub mod error;
pub mod memories;
pub mod migrations;
pub mod plugins;
pub mod policies;
pub mod schedules;
pub mod settings;
pub mod skills;
pub mod util;
pub mod work;

pub use approvals::ApprovalRepo;
pub use clients::{ClientDeclaration, ClientRepo};
pub use companions::CompanionRepo;
pub use error::{Result, StoreError};
pub use memories::MemoryRepo;
pub use migrations::{BASELINE_VERSION, VERSION};
pub use plugins::{PluginEntry, PluginRepo};
pub use policies::PolicyRepo;
pub use schedules::{
    Cron, NewSchedule, Schedule, ScheduleClock, ScheduleOccurrence, ScheduleRepo, next_occurrence,
};
pub use settings::SettingsRepo;
pub use skills::{Skill, SkillRepo, validate_skill_name};
pub use work::{WorkRepo, open_state};

use rusqlite::{Connection, OptionalExtension};
use std::{
    fmt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
}

/// The relational state store shared across repositories and host systems.
#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>);

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens `path`, validates/initializes the schema, and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Storage(format!("state directory: {error}")))?;
        }
        let mut connection = open_connection(path)?;
        migrations::validate_open_version(&connection)?;
        configure_connection(&connection)?;
        migrations::apply_migrations(&mut connection)?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    /// Borrows the primary SQLite connection under its mutual-exclusion lock.
    pub fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| StoreError::Storage("state database lock was poisoned".into()))
    }

    /// Opens a second read-write connection to the same database file with the
    /// same WAL pragmas, allowing background workers to avoid holding `conn()`.
    pub fn open_secondary(&self) -> Result<Connection> {
        let primary = self.conn()?;
        let path = primary
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                StoreError::Storage(
                    "the state database has no file a second connection could open".into(),
                )
            })?;
        let connection = open_connection(Path::new(path))?;
        configure_connection(&connection)?;
        Ok(connection)
    }

    /// Repository for approval requests.
    pub fn approval_repo(&self) -> ApprovalRepo<'_> {
        ApprovalRepo::new(self)
    }

    /// Repository for declared clients, activations, and device operations.
    pub fn client_repo(&self) -> ClientRepo<'_> {
        ClientRepo::new(self)
    }

    /// Repository for companion profiles and state revisions.
    pub fn companion_repo(&self) -> CompanionRepo<'_> {
        CompanionRepo::new(self)
    }

    /// Repository for episodic memories, vector backfill, and consolidation.
    pub fn memory_repo(&self) -> MemoryRepo<'_> {
        MemoryRepo::new(self)
    }

    /// Repository for installed plugin metadata, configuration, and isolated KV state.
    pub fn plugin_repo(&self) -> PluginRepo<'_> {
        PluginRepo::new(self)
    }

    /// Repository for saved policy rules and free-text policy instructions.
    pub fn policy_repo(&self) -> PolicyRepo<'_> {
        PolicyRepo::new(self)
    }

    /// Repository for scheduled reminders and materialized occurrences.
    pub fn schedule_repo(&self) -> ScheduleRepo<'_> {
        ScheduleRepo::new(self)
    }

    /// Repository for external connections and top-level settings revisions.
    pub fn settings_repo(&self) -> SettingsRepo<'_> {
        SettingsRepo::new(self)
    }

    /// Repository for companion-scoped procedural skills.
    pub fn skill_repo(&self) -> SkillRepo<'_> {
        SkillRepo::new(self)
    }

    /// Repository for commissioned work sessions and instruction runs.
    pub fn work_repo(&self) -> WorkRepo<'_> {
        WorkRepo::new(self)
    }

    pub(crate) fn fetch<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map).optional()?)
    }

    pub(crate) fn write_one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map)?)
    }

    pub(crate) fn fetch_all<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(sql)?;
        let rows = statement.query_map(params, map)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

/// Parses a stored JSON column.
pub(crate) fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|_| StoreError::Storage("stored JSON is corrupt".into()))
}

/// A stored value that does not match its domain type: the row is corrupt.
pub(crate) fn corrupt(index: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(what)),
    )
}

/// Reads a JSON column.
pub(crate) fn json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<T> {
    let index = row.as_ref().column_index(name)?;
    serde_json::from_str(&row.get::<_, String>(index)?).map_err(|_| corrupt(index, what))
}

/// Reads an optional JSON column.
pub(crate) fn optional_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<Option<T>> {
    let index = row.as_ref().column_index(name)?;
    row.get::<_, Option<String>>(index)?
        .as_deref()
        .map(|text| serde_json::from_str(text).map_err(|_| corrupt(index, what)))
        .transpose()
}

/// Reads a Serde enum-name column without constructing a JSON string.
pub(crate) fn enum_name_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<T> {
    let index = row.as_ref().column_index(name)?;
    let text: String = row.get(index)?;
    T::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&text))
        .map_err(|_| corrupt(index, what))
}

pub(crate) fn to_json(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| StoreError::Storage(error.to_string()))
}

/// Turns a zero-row write into the conflict it represents.
pub(crate) fn require(changed: usize, message: &str) -> Result<()> {
    if changed == 0 {
        return Err(StoreError::Conflict(message.into()));
    }
    Ok(())
}

/// Turns a zero-row write into the missing row it represents.
pub(crate) fn require_found(changed: usize, message: &str) -> Result<()> {
    if changed == 0 {
        return Err(StoreError::NotFound(message.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_columns_keep_domain_values_and_json_null_as_text() {
        use serde_json::{Value, json};
        let connection = Connection::open_in_memory().unwrap();
        let stored_null: Option<Value> = connection
            .query_row("SELECT 'null' AS value", [], |row| {
                optional_json_column(row, "value", "bad JSON")
            })
            .unwrap();
        assert_eq!(stored_null, Some(Value::Null));
        let object: Value = connection
            .query_row(
                r#"SELECT '{"tool":"device_request"}' AS value"#,
                [],
                |row| json_column(row, "value", "bad JSON"),
            )
            .unwrap();
        assert_eq!(object, json!({"tool": "device_request"}));
        let domain: Vec<String> = connection
            .query_row(r#"SELECT '["tea","coffee"]' AS value"#, [], |row| {
                json_column(row, "value", "bad domain")
            })
            .unwrap();
        assert_eq!(domain, ["tea", "coffee"]);
        let absent: Option<Value> = connection
            .query_row("SELECT NULL AS value", [], |row| {
                optional_json_column(row, "value", "bad JSON")
            })
            .unwrap();
        assert_eq!(absent, None);
    }

    #[test]
    fn serde_enum_columns_preserve_names_and_reject_unknown_values() {
        use ene_domain::{ConnectionKind, DeviceStatus, RunState, WorkStatus};
        use ene_policy::{ApprovalStatus, PolicyEffect};
        fn check<
            T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        >(
            values: &[T],
        ) {
            let connection = Connection::open_in_memory().unwrap();
            for value in values {
                let name = serde_json::to_value(value).unwrap();
                let parsed: T = connection
                    .query_row("SELECT ?1 AS name", [name.as_str().unwrap()], |row| {
                        enum_name_column(row, "name", "unknown enum")
                    })
                    .unwrap();
                assert_eq!(&parsed, value);
            }
            for name in ["unknown", "", "Pending", " pending"] {
                let error = connection
                    .query_row::<T, _, _>("SELECT 1, ?1 AS name", [name], |row| {
                        enum_name_column(row, "name", "unknown enum")
                    })
                    .unwrap_err();
                assert!(matches!(
                    error,
                    rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, _)
                ));
            }
        }
        check(&[
            PolicyEffect::Allow,
            PolicyEffect::Deny,
            PolicyEffect::Confirm,
        ]);
        check(&[
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
        ]);
        check(&[
            DeviceStatus::Pending,
            DeviceStatus::Dispatched,
            DeviceStatus::Completed,
            DeviceStatus::Failed,
            DeviceStatus::Uncertain,
        ]);
        check(&[
            ConnectionKind::Llm,
            ConnectionKind::Tts,
            ConnectionKind::Stt,
            ConnectionKind::Embedding,
            ConnectionKind::Mcp,
            ConnectionKind::Acp,
            ConnectionKind::Search,
        ]);
        check(&[WorkStatus::Idle, WorkStatus::Active, WorkStatus::Closed]);
        check(&[
            RunState::Starting,
            RunState::Running,
            RunState::WaitingInput,
            RunState::Completed,
            RunState::Cancelled,
            RunState::Failed,
        ]);
    }

    #[test]
    fn a_stop_state_this_host_no_longer_writes_decodes_as_cancelled() {
        use ene_domain::RunState;

        let connection = Connection::open_in_memory().unwrap();
        for name in ["cancelling", "cancellation_unconfirmed"] {
            let state: RunState = connection
                .query_row("SELECT ?1 AS name", [name], |row| {
                    enum_name_column(row, "name", "unknown run state")
                })
                .unwrap();
            assert_eq!(state, RunState::Cancelled, "{name}");
        }
    }

    #[test]
    fn primary_and_secondary_connections_share_options_and_transactions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let mut secondary = store.open_secondary().unwrap();
        let options = |connection: &Connection| {
            let journal: String = connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            let synchronous: i64 = connection
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .unwrap();
            let timeout: i64 = connection
                .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                .unwrap();
            (journal, synchronous, timeout)
        };
        assert_eq!(options(&store.conn().unwrap()), ("wal".into(), 1, 5000));
        assert_eq!(options(&secondary), ("wal".into(), 1, 5000));

        let tx = secondary.transaction().unwrap();
        tx.execute(
            "INSERT INTO user_profiles (id, name, updated_at) VALUES ('uncommitted', 'hidden', 'now')",
            [],
        )
        .unwrap();
        let visible_inside_tx: i64 = tx
            .query_row("SELECT COUNT(*) FROM user_profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible_inside_tx, 1);
        let visible_to_store: i64 = store
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM user_profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible_to_store, 0);
        tx.rollback().unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM user_profiles", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        secondary
            .execute(
                "INSERT INTO user_profiles (id, name, updated_at) VALUES ('committed', 'visible', 'now')",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row("SELECT name FROM user_profiles", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "visible"
        );
    }

    #[test]
    fn corrupt_json_reports_the_column_without_disclosing_its_contents() {
        let connection = Connection::open_in_memory().unwrap();
        let malformed = connection
            .query_row("SELECT 1, ?1 AS config", ["private text"], |row| {
                json_column::<serde_json::Value>(row, "config", "stored config is corrupt")
            })
            .unwrap_err();
        let wrong_type = connection
            .query_row("SELECT 1, ?1 AS config", ["\"private text\""], |row| {
                json_column::<Vec<String>>(row, "config", "stored config is corrupt")
            })
            .unwrap_err();
        for error in [malformed, wrong_type] {
            assert!(matches!(
                error,
                rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, _)
            ));
            let error = StoreError::from(error).to_string();
            assert!(error.contains("stored config is corrupt"));
            assert!(!error.contains("private text"));
        }
    }

    #[test]
    fn repeated_queries_keep_parameters_results_and_errors_separate() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch(
                "CREATE TABLE query_values (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )
            .unwrap();
        let insert = "INSERT INTO query_values (value) VALUES (?1) RETURNING id";
        for (value, expected) in [("first", 1), ("second", 2)] {
            let id: i64 = store.write_one(insert, [value], |row| row.get(0)).unwrap();
            assert_eq!(id, expected);
        }
        let select = "SELECT value FROM query_values WHERE id = ?1";
        for (id, expected) in [(2, Some("second")), (99, None), (1, Some("first"))] {
            let value: Option<String> = store.fetch(select, [id], |row| row.get(0)).unwrap();
            assert_eq!(value.as_deref(), expected);
        }
        assert!(store.fetch::<i64>(select, [1], |row| row.get(0)).is_err());
        let value: Option<String> = store.fetch(select, [2], |row| row.get(0)).unwrap();
        assert_eq!(value.as_deref(), Some("second"));
        let select_all = "SELECT value FROM query_values WHERE id >= ?1 ORDER BY id";
        for (minimum, expected) in [(1, vec!["first", "second"]), (2, vec!["second"])] {
            let values: Vec<String> = store
                .fetch_all(select_all, [minimum], |row| row.get(0))
                .unwrap();
            assert_eq!(values, expected);
        }
        let id: i64 = store
            .write_one(insert, ["third"], |row| row.get(0))
            .unwrap();
        assert_eq!(id, 3);
    }

    #[test]
    fn an_older_schema_is_refused_instead_of_being_read_as_current() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("CREATE TABLE companions (id TEXT PRIMARY KEY, name TEXT)")
                .unwrap();
        }
        assert!(
            Store::open(&path).is_err(),
            "a version-0 database with tables is not the current baseline"
        );
        let fresh = temp.path().join("fresh.sqlite3");
        Store::open(&fresh).unwrap();
    }

    #[test]
    fn a_database_round_trips_and_refuses_a_newer_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            VERSION
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        store
            .conn()
            .unwrap()
            .pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        drop(store);
        assert!(Store::open(&path).is_err());
    }
}
