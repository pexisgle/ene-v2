//! The one SQLite connection, its baseline schema and small helpers.
pub(crate) mod approvals;
pub(crate) mod clients;
pub(crate) mod companions;
pub(crate) mod memories;
pub(crate) mod plugins;
pub(crate) mod policies;
pub(crate) mod schedules;
pub(crate) mod settings;
pub(crate) mod work;

pub(crate) use super::error::{HostError, Result};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// A baseline schema version. Databases with another version are refused, as
/// this alpha has no in-place migration path.
pub(crate) const VERSION: i64 = 6;

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
}

/// The relational state, shared by every system on the Rig thread.
#[derive(Clone, bevy_ecs::prelude::Resource)]
pub(crate) struct Store(Arc<Mutex<Connection>>);

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| HostError::Storage(format!("state directory: {error}")))?;
        }
        let mut connection = open_connection(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > VERSION {
            return Err(HostError::Storage(format!(
                "state database version {version} is newer than this host ({VERSION})"
            )));
        }
        if version < VERSION {
            // Only an empty database is initialized as this baseline. One with
            // tables is from an older development schema, which this alpha
            // does not migrate in place: refusing is honest, treating its
            // tables as current would fail later with a corrupt-state error.
            let tables: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if tables > 0 {
                return Err(HostError::Storage(format!(
                    "state database version {version} predates this host's baseline; it is not migrated in place"
                )));
            }
        }
        configure_connection(&connection)?;
        // The tables and the version commit together, so a crash cannot leave
        // a current schema that the next start refuses as an old one.
        let transaction = connection.transaction()?;
        transaction.execute_batch(include_str!("schema.sql"))?;
        transaction.pragma_update(None, "user_version", VERSION)?;
        transaction.commit()?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    /// Borrows the connection. Every host access runs on the Rig thread, so
    /// the lock is never contended; a poisoned lock means state is unusable.
    pub(crate) fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| HostError::Storage(("state database lock was poisoned").into()))
    }

    /// Runs a query that returns at most one row.
    fn fetch<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map).optional()?)
    }

    /// Runs one statement that must return exactly one row.
    fn write_one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(sql)?;
        Ok(statement.query_row(params, map)?)
    }

    /// Runs a query that returns every matching row.
    fn fetch_all<T>(
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

/// The current time in UTC, RFC 3339.
pub(crate) fn now() -> String {
    // The stored columns are compared as text, so every stamp keeps the same
    // second precision the schedule rows are written with.
    format!("{:.0}", Timestamp::now())
}

/// Parses RFC 3339 into seconds since the Unix epoch.
pub(crate) fn epoch(text: &str) -> Option<i64> {
    text.parse::<Timestamp>().ok().map(Timestamp::as_second)
}

/// The SHA-256 of `bytes`, used for action payloads and idempotency hashes.
pub(crate) fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Opaque ids are 1..=128 printable ASCII characters. Anything else is a
/// client mistake, not a lookup miss.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.chars().count() > 128
        || id.bytes().any(|byte| !(0x21..=0x7e).contains(&byte))
    {
        return Err(HostError::Invalid(
            ("id must be 1-128 printable ASCII characters").into(),
        ));
    }
    Ok(())
}

/// Parses a stored JSON column.
pub(crate) fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|_| HostError::Storage(("stored JSON is corrupt").into()))
}

/// A stored value that does not match its domain type: the row is corrupt.
fn corrupt(index: usize, what: &str) -> rusqlite::Error {
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
    serde_json::to_string(value).map_err(|error| HostError::Storage(error.to_string()))
}

/// The caller's stamp, or `fallback` (one `now()` shared by a row) when the
/// caller did not set one.
pub(crate) fn or_stamp(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// Turns a zero-row write into the conflict it represents.
pub(crate) fn require(changed: usize, message: &str) -> Result<()> {
    if changed == 0 {
        return Err(HostError::Conflict((message).into()));
    }
    Ok(())
}

/// Turns a zero-row write into the missing row it represents.
pub(crate) fn require_found(changed: usize, message: &str) -> Result<()> {
    if changed == 0 {
        return Err(HostError::NotFound((message).into()));
    }
    Ok(())
}

/// Truncates `value` to at most `limit` characters.
pub(crate) fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_columns_keep_domain_values_and_json_null_as_text() {
        use serde_json::{Value, json};
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE json_values (value TEXT NOT NULL, optional TEXT)")
            .unwrap();
        for value in [
            Value::Null,
            json!(1),
            json!(1.0),
            json!(["tea"]),
            json!({"nested": null}),
        ] {
            connection.execute("DELETE FROM json_values", []).unwrap();
            connection
                .execute(
                    "INSERT INTO json_values VALUES (?1, ?2)",
                    [to_json(&value).unwrap(), "null".into()],
                )
                .unwrap();
            let stored: (String, Value, Option<Value>) = connection
                .query_row(
                    "SELECT typeof(value), value, optional FROM json_values",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            json_column(row, "value", "bad JSON")?,
                            optional_json_column(row, "optional", "bad JSON")?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(stored, ("text".into(), value, Some(Value::Null)));
        }
        let domain: Vec<String> = connection
            .query_row("SELECT '[\"tea\",\"coffee\"]' AS value", [], |row| {
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
        use crate::host::{
            client::DeviceStatus,
            permission::{ApprovalStatus, PolicyEffect},
            settings::ConnectionKind,
            work::{RunState, WorkStatus},
        };
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

    /// A run stopped by an older host still opens: its state reads as the one a
    /// stop reaches now instead of failing the row.
    #[test]
    fn a_stop_state_this_host_no_longer_writes_decodes_as_cancelled() {
        use crate::host::work::RunState;

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
            let sync: i64 = connection
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .unwrap();
            let busy: i64 = connection
                .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                .unwrap();
            (journal, sync, busy)
        };
        assert_eq!(options(&store.conn().unwrap()), ("wal".into(), 1, 5000));
        assert_eq!(options(&secondary), ("wal".into(), 1, 5000));
        let transaction = secondary
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        transaction.execute("INSERT INTO user_profiles (id, name, updated_at) VALUES ('rollback', 'discard', 'now')", []).unwrap();
        transaction.rollback().unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM user_profiles", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        secondary.execute("INSERT INTO user_profiles (id, name, updated_at) VALUES ('committed', 'visible', 'now')", []).unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row("SELECT name FROM user_profiles", [], |row| row
                    .get::<_, String>(0))
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
            let error = HostError::from(error).to_string();
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

    /// A database from before the baseline is refused instead of being read
    /// as the current schema, and an empty one is initialized.
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
                .query_row("PRAGMA user_version", [], |row| { row.get::<_, i64>(0) })
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
