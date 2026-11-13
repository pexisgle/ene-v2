//! The one SQLite connection, its baseline schema and small helpers.
pub(crate) mod approvals;
pub(crate) mod clients;
pub(crate) mod companions;
pub(crate) mod memories;
pub(crate) mod policies;
pub(crate) mod schedules;
pub(crate) mod settings;
pub(crate) mod work;

pub(crate) use super::error::{HostError, Result};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// A baseline schema version. Databases with another version are refused, as
/// this alpha has no in-place migration path.
pub(crate) const VERSION: i64 = 1;

/// The relational state, shared by every system on the Rig thread.
#[derive(Clone, bevy_ecs::prelude::Resource)]
pub(crate) struct Store(Arc<Mutex<Connection>>);

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > VERSION {
            return Err(HostError::storage(format!(
                "state database version {version} is newer than this host ({VERSION})"
            )));
        }
        connection.execute_batch(include_str!("schema.sql"))?;
        connection.pragma_update(None, "user_version", VERSION)?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    /// Borrows the connection. Every host access runs on the Rig thread, so
    /// the lock is never contended; a poisoned lock means state is unusable.
    pub(crate) fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| HostError::storage("state database lock was poisoned"))
    }

    /// Runs a query that returns at most one row.
    fn fetch<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        let connection = self.conn()?;
        Ok(connection.query_row(sql, params, map).optional()?)
    }

    /// Runs one statement that must return exactly one row.
    fn write_one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        Ok(self.conn()?.query_row(sql, params, map)?)
    }

    /// Runs a query that returns every matching row.
    fn fetch_all<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params, map)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

/// The current time in UTC, RFC 3339.
pub(crate) fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Parses RFC 3339 into seconds since the Unix epoch.
pub(crate) fn epoch(text: &str) -> Option<i64> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(|time| time.unix_timestamp())
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
        return Err(HostError::invalid(
            "id must be 1-128 printable ASCII characters",
        ));
    }
    Ok(())
}

/// Parses a stored JSON column.
pub(crate) fn json(text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text).map_err(|_| HostError::storage("stored JSON is corrupt"))
}

/// A stored value that does not match its domain type: the row is corrupt.
pub(crate) fn corrupt(text: &str, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        text.len(),
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(what)),
    )
}

/// Parses a stored JSON column as `T`, reporting a parse failure as corruption.
pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(
    text: &str,
    what: &str,
) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|_| corrupt(text, what))
}

/// Parses a stored enum name, reporting an unknown name as corruption.
pub(crate) fn parse_enum<T>(
    text: &str,
    what: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    parse(text).ok_or_else(|| corrupt(text, what))
}

/// Reads a JSON column.
pub(crate) fn json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<T> {
    parse_json(&row.get::<_, String>(name)?, what)
}

/// Reads an optional JSON column.
pub(crate) fn optional_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(name)?
        .as_deref()
        .map(|text| parse_json(text, what))
        .transpose()
}

/// Reads an enum-name column.
pub(crate) fn enum_column<T>(
    row: &rusqlite::Row<'_>,
    name: &str,
    what: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    parse_enum(&row.get::<_, String>(name)?, what, parse)
}

pub(crate) fn to_json(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
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
        return Err(HostError::conflict(message));
    }
    Ok(())
}

/// Turns a zero-row write into the missing row it represents.
pub(crate) fn require_found(changed: usize, message: &str) -> Result<()> {
    if changed == 0 {
        return Err(HostError::not_found(message));
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
    fn ids_and_times_are_strict() {
        for id in ["a", "companion-1", "A_b.c-9"] {
            assert!(validate_id(id).is_ok(), "{id}");
        }
        for id in ["", "has space", "line\nbreak", "日本語"] {
            assert!(validate_id(id).is_err(), "{id:?}");
        }
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("not a time"), None);
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
