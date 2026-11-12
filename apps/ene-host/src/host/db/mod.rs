//! The typed SQLite store. The ECS owns the one connection.
mod clients;
mod companions;
mod memories;
mod receipts;
mod settings;
mod work;

pub(crate) use super::error::Result;
use super::{companion::Companion, error::HostError};
use bevy_ecs::prelude::Resource;
use rusqlite::{Connection, OptionalExtension};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("schema.sql");

/// The one SQLite connection, shared by the ECS systems.
///
/// The connection is synchronous and fast enough for the host's PK, FTS5 and
/// rule lookups; it never leaves the Rig thread. The mutex is only needed so
/// the Bevy resource can be shared by systems.
#[derive(Clone, Resource)]
pub(crate) struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        initialize(&mut connection)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.seed()?;
        Ok(store)
    }

    /// Runs `operation` on the ECS thread.
    fn call<T>(&self, operation: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| HostError::internal("state storage lock is poisoned"))?;
        operation(&mut connection)
    }

    fn seed(&self) -> Result<()> {
        if self.companions()?.is_empty() {
            let now = now();
            let companion = Companion::seed(new_id(), now);
            self.save_companion(&companion)?;
        }
        Ok(())
    }
}

fn initialize(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        if has_tables(connection)? {
            return Err(HostError::storage(
                "state database is not compatible; delete the state directory",
            ));
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    } else if version != SCHEMA_VERSION {
        return Err(HostError::storage(
            "state database schema is newer than this host",
        ));
    }
    if !has_schema(connection)? {
        return Err(HostError::storage(
            "state database is not the ene host schema",
        ));
    }
    ensure_settings(connection)
}

fn has_tables(connection: &Connection) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn has_schema(connection: &Connection) -> Result<bool> {
    for table in [
        "settings",
        "companions",
        "connections",
        "activations",
        "policy_rules",
        "approval_requests",
        "schedules",
        "clients",
        "work_sessions",
        "work_inquiries",
        "device_operations",
        "receipts",
        "mind_operations",
        "memories",
        "memories_fts",
        "memory_edges",
        "consolidation_state",
    ] {
        let found: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if found.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn ensure_settings(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT OR IGNORE INTO settings(id, revision, updated_at) VALUES (1, 1, ?1)",
        [now()],
    )?;
    transaction.commit()?;
    Ok(())
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
        .ok_or_else(|| HostError::conflict("revision exhausted"))
}

pub(crate) fn digest(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value))
}

pub(crate) fn validate_id(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 200
        && !value.trim().is_empty()
        && !value.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(HostError::invalid("invalid identifier"))
    }
}

pub(crate) fn validate_key(value: &str) -> Result<()> {
    if !value.is_empty() && value.len() <= 200 && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        Ok(())
    } else {
        Err(HostError::invalid("invalid request key"))
    }
}

pub(crate) fn parse_json(value: &str) -> Result<serde_json::Value> {
    if value.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(value).map_err(|_| HostError::invalid("invalid JSON document"))
}
