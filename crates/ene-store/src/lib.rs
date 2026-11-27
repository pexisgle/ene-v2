//! SQLite persistence and domain repositories.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.

pub mod approvals;
pub mod clients;
pub mod companions;
pub mod credentials;
pub mod error;
pub mod memories;
pub mod plugins;
pub mod policies;
mod row;
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
pub use plugins::{PluginEntry, PluginRepo};
pub use policies::PolicyRepo;
pub use schedules::{NewSchedule, Schedule, ScheduleClock, ScheduleOccurrence, ScheduleRepo};
pub use settings::SettingsRepo;
pub use skills::{Skill, SkillRepo, validate_skill_name};
pub use work::{WorkRepo, open_state};

use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::{fmt, path::Path, sync::Arc};

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn schema_version(connection: &Connection) -> Result<i64> {
    let version = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        1 => Ok(version),
        0 if !connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
            [],
            |row| row.get::<_, bool>(0),
        )? =>
        {
            Ok(version)
        }
        _ => Err(StoreError::Storage(format!(
            "unsupported state database version {version}; a new database is required"
        ))),
    }
}

fn configure_connection(connection: &Connection) -> rusqlite::Result<()> {
    // `foreign_keys` is per connection and off by default, so every connection
    // that can write declares it here. `temp_store` and `cache_size` keep the
    // sort buffers of the repository queries off disk.
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16000;",
    )
}

/// The relational state store shared across repositories and host systems.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    grants: Arc<ene_policy::CapabilityGrants>,
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens current state or initializes an empty database.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_grants(path, ene_policy::CapabilityGrants::default())
    }

    /// Opens state with the host's startup capability policy.
    pub fn open_with_grants(path: &Path, grants: ene_policy::CapabilityGrants) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Storage(format!("state directory: {error}")))?;
        }
        let mut connection = open_connection(path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if schema_version(&transaction)? == 0 {
            transaction.execute_batch(include_str!("schema.sql"))?;
        }
        transaction.commit()?;
        configure_connection(&connection)?;
        Ok(Self::from_connection(connection, Arc::new(grants)))
    }

    /// Opens existing learned state without creating a database.
    pub fn open_for_history_reset(path: &Path) -> Result<Option<Self>> {
        if !path
            .try_exists()
            .map_err(|error| StoreError::Storage(format!("state database: {error}")))?
        {
            return Ok(None);
        }
        let connection = open_connection(path)?;
        if schema_version(&connection)? == 0 {
            return Ok(None);
        }
        configure_connection(&connection)?;
        Ok(Some(Self::from_connection(connection, Arc::default())))
    }

    fn from_connection(connection: Connection, grants: Arc<ene_policy::CapabilityGrants>) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
            grants,
        }
    }

    /// Borrows the primary SQLite connection under its mutual-exclusion lock.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock()
    }

    /// Opens a second read-write connection to the same database file with the
    /// same WAL pragmas, allowing background workers to avoid holding `conn()`.
    pub fn open_secondary(&self) -> Result<Self> {
        let primary = self.conn();
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
        Ok(Self::from_connection(connection, self.grants.clone()))
    }

    /// Erases learned state before the stopped host releases its reset marker.
    pub fn reset_learned_state(&self) -> Result<()> {
        let mut connection = self.conn();
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "DELETE FROM memory_edges;
             DELETE FROM memory_injections;
             DELETE FROM mind_operations;
             DELETE FROM skills;
             DELETE FROM memories;
             INSERT INTO memories_fts(memories_fts) VALUES ('delete-all');
             DELETE FROM device_operations;
             DELETE FROM approval_requests;
             DELETE FROM work_runs;
             DELETE FROM work_sessions;
             DELETE FROM compaction_checkpoints;
             DELETE FROM schedule_occurrences WHERE status = 'delivered';",
        )?;
        let empty = to_json(&ene_mind::State::default())?;
        transaction.execute(
            "UPDATE companions SET state = ?1, state_revision = state_revision + 1 WHERE state <> ?1",
            [empty],
        )?;
        transaction.commit()?;
        // FTS5 clears its own segments above. Vacuum removes discarded table
        // pages, then a truncate checkpoint removes their older WAL images.
        connection.execute_batch("VACUUM")?;
        let busy = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, bool>(0)
        })?;
        if busy {
            return Err(StoreError::Unavailable(
                "a database reader prevented history reset from truncating the WAL".into(),
            ));
        }
        Ok(())
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

    /// Repository for nonsecret credential publication and erasure metadata.
    pub fn credential_repo(&self) -> credentials::CredentialRepo<'_> {
        credentials::CredentialRepo::new(self)
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
        let connection = self.conn();
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map).optional()?)
    }

    pub(crate) fn write_one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        let connection = self.conn();
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map)?)
    }

    /// Runs `write` in one write transaction that takes the lock up front.
    ///
    /// `DEFERRED` (the default) starts as a reader and only upgrades on the
    /// first write, so two concurrent writers can both begin and then one gets
    /// `SQLITE_BUSY` that `busy_timeout` cannot retry. `Immediate` serialises
    /// writers through the lock at `BEGIN`, which is what every write path wants.
    pub(crate) fn write_tx<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.conn();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = write(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    pub(crate) fn fetch_all<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let connection = self.conn();
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

pub(crate) fn id_column<T: TryFrom<String, Error = ene_domain::DomainError>>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<T> {
    let index = row.as_ref().column_index(name)?;
    T::try_from(row.get::<_, String>(index)?).map_err(|_| corrupt(index, what))
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

pub(crate) fn to_json(value: &(impl serde::Serialize + ?Sized)) -> Result<String> {
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
        let secondary_store = store.open_secondary().unwrap();
        let mut secondary = secondary_store.conn();
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
        assert_eq!(options(&store.conn()), ("wal".into(), 1, 5000));
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
            .query_row("SELECT COUNT(*) FROM user_profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible_to_store, 0);
        tx.rollback().unwrap();
        assert_eq!(
            store
                .conn()
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
    fn a_write_transaction_survives_a_read_then_write_with_a_competing_commit() {
        // The repositories read and then write inside one transaction. With
        // `DEFERRED` the transaction opens as a reader, so a commit by another
        // connection lands between the read and the write; the upgrade then
        // fails with SQLITE_BUSY_SNAPSHOT, which `busy_timeout` never retries.
        // `Immediate` takes the write lock at `BEGIN`, so the competing writer
        // waits instead of invalidating this transaction's snapshot.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        store
            .conn()
            .execute_batch("CREATE TABLE busy_probe (id INTEGER PRIMARY KEY, who TEXT NOT NULL)")
            .unwrap();

        // Another connection commits inside the window this transaction leaves
        // open between its read and its write.
        let competitor_path = path.clone();
        let competitor = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let mut connection = Connection::open(&competitor_path).unwrap();
            connection
                .busy_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute("INSERT INTO busy_probe (who) VALUES ('competitor')", [])
                .unwrap();
            tx.commit().unwrap();
        });

        let written = store.write_tx(|tx| {
            // The read a repository does before it writes.
            let existing: i64 =
                tx.query_row("SELECT COUNT(*) FROM busy_probe", [], |row| row.get(0))?;
            std::thread::sleep(std::time::Duration::from_millis(400));
            // The write that must still be allowed to land.
            tx.execute("INSERT INTO busy_probe (who) VALUES ('reader-writer')", [])?;
            Ok(existing)
        });

        competitor.join().unwrap();
        let existing = written.expect("a read-then-write transaction must not fail busy");
        assert_eq!(existing, 0, "the read ran before either write");
        let total: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM busy_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2, "both the reader-writer and the competitor commit");
    }

    #[test]
    fn the_bulk_session_query_matches_the_per_companion_query() {
        // `snapshot` used to loop one `work_sessions(companion)` call per
        // companion. The bulk query replaces that loop, so its rows must group
        // by companion and stay newest-first within each companion.
        use ene_domain::{WorkSession, WorkStatus};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.work_repo();
        let stamp = |minute: u32| format!("2026-01-01T00:{minute:02}:00Z");
        for (id, companion, minute) in [
            ("a-old", "alpha", 1),
            ("a-new", "alpha", 5),
            ("b-only", "beta", 3),
            ("a-mid", "alpha", 3),
        ] {
            repo.insert_work_session(&WorkSession {
                id: id.into(),
                companion_id: ene_domain::CompanionId::parse(companion).unwrap(),
                title: id.into(),
                status: WorkStatus::Active,
                client_id: None,
                created_at: stamp(minute),
                updated_at: stamp(minute),
            })
            .unwrap();
        }

        let bulk = repo.all_work_sessions().unwrap();
        let seen: Vec<(&str, &str)> = bulk
            .iter()
            .map(|session| (session.companion_id.as_str(), session.id.as_str()))
            .collect();
        // The exact row order the query promises, stated once so a change to
        // the ORDER BY fails here rather than silently reordering a snapshot.
        assert_eq!(
            seen,
            [
                ("alpha", "a-new"),
                ("alpha", "a-mid"),
                ("alpha", "a-old"),
                ("beta", "b-only"),
            ],
            "the bulk query must group by companion, newest first"
        );

        // And it must agree with the per-companion query per companion.
        for companion in ["alpha", "beta"] {
            let one = repo.work_sessions(companion).unwrap();
            let many: Vec<&str> = bulk
                .iter()
                .filter(|session| session.companion_id.as_str() == companion)
                .map(|session| session.id.as_str())
                .collect();
            let expected: Vec<&str> = one.iter().map(|session| session.id.as_str()).collect();
            assert_eq!(
                many, expected,
                "{companion} must match the single-companion query"
            );
        }
    }

    #[test]
    fn repeated_queries_keep_parameters_results_and_errors_separate() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        store
            .conn()
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
    fn a_database_round_trips_and_refuses_the_legacy_ladder() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .conn()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        store
            .conn()
            .pragma_update(None, "user_version", 28)
            .unwrap();
        drop(store);
        assert!(Store::open(&path).is_err());
    }
}
