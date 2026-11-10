//! Durable SQLite WAL store for host documents, idempotency receipts and memory FTS5 index.
mod fts;

use connectrpc::ConnectError;
use ene_rpc as w;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    path::Path,
    sync::{Mutex, MutexGuard, TryLockError},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

pub(crate) type Result<T> = std::result::Result<T, ConnectError>;

/// The `meta.schema_baseline` a database written before migrations carries.
const BASELINE: &str = "2026-11-rewrite-1";

/// The schema, one entry per version: `PRAGMA user_version` is how many ran.
const MIGRATIONS: &[&str] = &[include_str!("migrations/001_baseline.sql")];

#[derive(Debug)]
struct StorageFailure;
impl std::fmt::Display for StorageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("state storage failed")
    }
}
impl Error for StorageFailure {}

pub(crate) fn storage(_: impl std::fmt::Display) -> ConnectError {
    ConnectError::unavailable("state storage failed; restart after repairing storage")
        .with_source(StorageFailure)
}

pub(crate) fn is_storage(error: &ConnectError) -> bool {
    error
        .source()
        .is_some_and(|source| source.is::<StorageFailure>())
}

pub(crate) fn invalid(message: &'static str) -> ConnectError {
    ConnectError::invalid_argument(message)
}
pub(crate) fn not_found(message: &'static str) -> ConnectError {
    ConnectError::not_found(message)
}
pub(crate) fn aborted(message: &'static str) -> ConnectError {
    ConnectError::aborted(message)
}
pub(crate) fn permission(message: &'static str) -> ConnectError {
    ConnectError::permission_denied(message)
}
pub(crate) fn unavailable(message: &'static str) -> ConnectError {
    ConnectError::unavailable(message)
}

pub(crate) fn validate_id(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 200
        && !value.trim().is_empty()
        && !value.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(invalid("invalid identifier"))
    }
}

pub(crate) fn validate_key(value: &str) -> Result<()> {
    if !value.is_empty() && value.len() <= 200 && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        Ok(())
    } else {
        Err(invalid("invalid request key"))
    }
}

pub(crate) fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

pub(crate) fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn next_revision(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| aborted("revision exhausted"))
}

pub(crate) fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub(crate) fn document(value: Value) -> w::JsonDocument {
    w::JsonDocument {
        json: value.to_string(),
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReceiptStatus {
    Pending,
    Accepted,
    NotDelivered,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub(crate) hash: String,
    pub(crate) status: ReceiptStatus,
    pub(crate) response: Value,
    pub(crate) updated_at: String,
}

pub(crate) struct Database(pub(super) Mutex<Connection>);
pub(crate) struct Tx<'a>(pub(super) rusqlite::Transaction<'a>);

impl Database {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path).map_err(storage)?;
        initialize(Some(path), &mut connection)?;
        Ok(Self(Mutex::new(connection)))
    }

    /// Locks the connection, refusing a re-entry instead of deadlocking.
    ///
    /// The Rig thread is the only user, so a busy mutex means the same
    /// thread asked twice, such as `get` inside a `transaction` closure.
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        match self.0.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(error)) => Err(storage(error)),
            Err(TryLockError::WouldBlock) => Err(ConnectError::internal("database re-entered")),
        }
    }

    pub(crate) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        get_doc(&*self.lock()?, key)
    }

    pub(crate) fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        list_docs(&*self.lock()?, prefix)
    }

    pub(crate) fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        put_doc(&*self.lock()?, key, value)
    }

    pub(crate) fn check_receipt(
        &self,
        scope: &str,
        request_key: &str,
        hash: &str,
    ) -> Result<Option<Value>> {
        let Some(receipt) = self.get::<Receipt>(&format!("receipt:{scope}:{request_key}"))? else {
            return Ok(None);
        };
        if receipt.hash != hash {
            return Err(ConnectError::already_exists(
                "request key was used with different content",
            ));
        }
        match receipt.status {
            ReceiptStatus::Accepted => Ok(Some(receipt.response)),
            ReceiptStatus::Pending | ReceiptStatus::NotDelivered => Err(unavailable(
                "previous acceptance was not confirmed; inspect state before sending a new key",
            )),
        }
    }

    pub(crate) fn transaction<T>(&self, operation: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.lock()?;
        let tx = Tx(db.transaction().map_err(storage)?);
        let value = operation(&tx)?;
        tx.0.commit().map_err(storage)?;
        Ok(value)
    }

    pub(crate) fn host_id(&self) -> Result<String> {
        self.meta("host_id")
    }

    pub(crate) fn environment_id(&self) -> Result<String> {
        self.meta("environment_id")
    }

    fn meta(&self, key: &str) -> Result<String> {
        let db = self.lock()?;
        db.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .map_err(storage)
    }
}

impl Tx<'_> {
    pub(crate) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        get_doc(&self.0, key)
    }
    pub(crate) fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        list_docs(&self.0, prefix)
    }
    pub(crate) fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        put_doc(&self.0, key, value)
    }
    pub(crate) fn delete(&self, key: &str) -> Result<()> {
        self.0
            .execute("DELETE FROM documents WHERE key=?1", [key])
            .map_err(storage)?;
        Ok(())
    }
}

fn get_doc<T: DeserializeOwned>(db: &Connection, key: &str) -> Result<Option<T>> {
    let value: Option<String> = db
        .query_row("SELECT value FROM documents WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(storage)?;
    value
        .map(|value| serde_json::from_str(&value).map_err(storage))
        .transpose()
}

fn list_docs<T: DeserializeOwned>(db: &Connection, prefix: &str) -> Result<Vec<(String, T)>> {
    // A prefix is a range of the key index; `substr` would scan the table.
    // U+10FFFF is the largest character, so every longer key that starts
    // with the prefix sorts below it.
    let end = format!("{prefix}\u{10FFFF}");
    let mut statement = db
        .prepare("SELECT key, value FROM documents WHERE key >= ?1 AND key < ?2 ORDER BY key")
        .map_err(storage)?;
    statement
        .query_map([prefix, end.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(storage)?
        .map(|row| {
            let (key, value) = row.map_err(storage)?;
            Ok((key, serde_json::from_str(&value).map_err(storage)?))
        })
        .collect()
}

fn put_doc<T: Serialize>(db: &Connection, key: &str, value: &T) -> Result<()> {
    let value = serde_json::to_string(value).map_err(storage)?;
    db.execute(
        "INSERT INTO documents(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )
    .map_err(storage)?;
    Ok(())
}

fn initialize(path: Option<&Path>, db: &mut Connection) -> Result<()> {
    migrate(db, path, MIGRATIONS)
}

/// Applies every migration the database has not run yet, one transaction each.
///
/// `user_version` is the number of migrations applied. A database from the
/// 2026-11-09 rewrite carries its baseline in `meta` and no version: it is
/// adopted, not refused. Anything else unknown is refused rather than written.
fn migrate(db: &mut Connection, path: Option<&Path>, migrations: &[&str]) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .map_err(storage)?;
    let mut version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(storage)?;
    if version == 0 {
        if baseline(db)?.as_deref() == Some(BASELINE) {
            version = 1;
            db.pragma_update(None, "user_version", 1).map_err(storage)?;
        } else if has_tables(db)? {
            return Err(storage("database predates this host's baseline"));
        }
    }
    if version < 0 || version as usize > migrations.len() {
        return Err(storage("database schema is newer than this host"));
    }
    for (index, sql) in migrations.iter().enumerate().skip(version as usize) {
        let next = index + 1;
        if index > 0 {
            backup(db, path, version)?;
        }
        let tx = db.transaction().map_err(storage)?;
        tx.execute_batch(sql).map_err(storage)?;
        tx.pragma_update(None, "user_version", next as i64)
            .map_err(storage)?;
        tx.commit().map_err(storage)?;
    }
    ensure_meta(db)
}

fn baseline(db: &Connection) -> Result<Option<String>> {
    let has_meta: Option<String> = db
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    if has_meta.is_none() {
        return Ok(None);
    }
    db.query_row(
        "SELECT value FROM meta WHERE key='schema_baseline'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(storage)
}

fn has_tables(db: &Connection) -> Result<bool> {
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    Ok(tables > 0)
}

/// Copies the database beside itself before a later migration changes it.
fn backup(db: &Connection, path: Option<&Path>, version: i64) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let target = path.with_file_name(format!("{name}.bak-v{version}"));
    let _ = std::fs::remove_file(&target);
    db.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])
        .map_err(storage)?;
    Ok(())
}

fn ensure_meta(db: &mut Connection) -> Result<()> {
    let tx = db.transaction().map_err(storage)?;
    for key in ["host_id", "environment_id"] {
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)",
            params![key, new_id()],
        )
        .map_err(storage)?;
    }
    tx.commit().map_err(storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(db: &Connection) -> i64 {
        db.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    fn meta(db: &Connection, key: &str) -> Option<String> {
        db.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .unwrap()
    }

    #[test]
    fn a_fresh_database_gets_one_baseline_and_reopens_without_reseeding() {
        let mut db = Connection::open_in_memory().unwrap();
        initialize(None, &mut db).unwrap();
        assert_eq!(version(&db), 1);
        let host = meta(&db, "host_id").unwrap();
        initialize(None, &mut db).unwrap();
        assert_eq!(version(&db), 1);
        assert_eq!(meta(&db, "host_id"), Some(host));
    }

    #[test]
    fn a_foreign_database_is_refused() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY);")
            .unwrap();
        assert!(initialize(None, &mut db).is_err());
    }

    #[test]
    fn a_pre_migration_baseline_is_adopted_instead_of_refused() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(MIGRATIONS[0]).unwrap();
        for (key, value) in [("schema_baseline", BASELINE), ("host_id", "kept-host")] {
            db.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .unwrap();
        }
        initialize(None, &mut db).unwrap();
        assert_eq!(version(&db), 1);
        assert_eq!(meta(&db, "host_id").as_deref(), Some("kept-host"));
        assert!(meta(&db, "environment_id").is_some());
    }

    #[test]
    fn a_later_migration_keeps_data_and_a_failed_one_changes_nothing() {
        const ONE: &str = "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); \
             CREATE TABLE kit (id INTEGER PRIMARY KEY, value TEXT NOT NULL);";
        const TWO: &str = "INSERT INTO kit(value) VALUES ('one');";
        const BROKEN: &str = "ALTER TABLE kit ADD COLUMN extra TEXT; SELECT nope FROM nowhere;";
        let mut db = Connection::open_in_memory().unwrap();
        migrate(&mut db, None, &[ONE, TWO]).unwrap();
        assert_eq!(version(&db), 2);
        let kept: String = db
            .query_row("SELECT value FROM kit WHERE id=1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, "one");

        let error = migrate(&mut db, None, &[ONE, TWO, BROKEN]);
        assert!(error.is_err());
        assert_eq!(
            version(&db),
            2,
            "a failed migration must not bump the version"
        );
        let kept: String = db
            .query_row("SELECT value FROM kit WHERE id=1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, "one");
        let extra: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_table_info('kit') WHERE name='extra'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(extra, 0, "a failed migration must roll back");
    }

    #[test]
    fn a_database_from_a_newer_host_is_refused() {
        let mut db = Connection::open_in_memory().unwrap();
        initialize(None, &mut db).unwrap();
        db.pragma_update(None, "user_version", 99).unwrap();
        assert!(initialize(None, &mut db).is_err());
    }

    #[test]
    fn a_database_read_inside_a_transaction_is_refused_instead_of_deadlocking() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.sqlite3")).unwrap();
        let nested = db.transaction(|_| db.get::<Value>("host_id"));
        let error = nested.expect_err("re-entrant access must fail");
        assert_eq!(error.code, connectrpc::ErrorCode::Internal);
    }

    #[test]
    fn a_list_returns_the_keys_under_its_prefix_and_no_other() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.sqlite3")).unwrap();
        for key in ["work:1", "work:2", "worker:1", "receipt:work:1:x"] {
            db.put(key, &Value::from(key)).unwrap();
        }
        let keys: Vec<String> = db
            .list::<Value>("work:")
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, ["work:1", "work:2"]);
    }
}
