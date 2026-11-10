//! Product state and acceptance receipts; conversations remain in Rig's log.
use std::{path::Path, sync::Mutex};

use connectrpc::ConnectError;
use rusqlite::{Connection, OptionalExtension};
use serde::{Serialize, de::DeserializeOwned};

use super::{Receipt, Work};
use crate::proto::{chat::v1 as w, settings::v1::Settings};

pub(super) type Result<T> = std::result::Result<T, ConnectError>;

pub(super) struct Database(Mutex<Connection>);

#[derive(Debug)]
struct StorageFailure;
impl std::fmt::Display for StorageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("state storage failed")
    }
}
impl std::error::Error for StorageFailure {}

pub(super) fn storage(_: impl std::fmt::Display) -> ConnectError {
    ConnectError::unavailable("state storage failed; restart after repairing storage")
        .with_source(StorageFailure)
}

pub(super) fn is_storage(error: &ConnectError) -> bool {
    use std::error::Error;
    error
        .source()
        .is_some_and(|source| source.is::<StorageFailure>())
}

impl Database {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(storage)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS documents (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(storage)?;
        Ok(Self(Mutex::new(db)))
    }

    pub(super) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let db = self.0.lock().map_err(storage)?;
        let value: Option<String> = db
            .query_row("SELECT value FROM documents WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(storage)?;
        value
            .map(|json| serde_json::from_str(&json).map_err(storage))
            .transpose()
    }

    pub(super) fn put(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let json = serde_json::to_string(value).map_err(storage)?;
        let db = self.0.lock().map_err(storage)?;
        db.execute(
            "INSERT OR REPLACE INTO documents (key, value) VALUES (?1, ?2)",
            (key, json),
        )
        .map_err(storage)?;
        Ok(())
    }

    pub(super) fn put_pair(
        &self,
        k1: &str,
        v1: &impl Serialize,
        k2: &str,
        v2: &impl Serialize,
    ) -> Result<()> {
        let j1 = serde_json::to_string(v1).map_err(storage)?;
        let j2 = serde_json::to_string(v2).map_err(storage)?;
        let mut db = self.0.lock().map_err(storage)?;
        let tx = db.transaction().map_err(storage)?;
        for (k, v) in [(k1, j1), (k2, j2)] {
            tx.execute(
                "INSERT OR REPLACE INTO documents (key, value) VALUES (?1, ?2)",
                (k, v),
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)
    }

    pub(super) fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let db = self.0.lock().map_err(storage)?;
        let mut statement = db
            .prepare(
                "SELECT key,value FROM documents WHERE substr(key,1,length(?1))=?1 ORDER BY key",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([prefix], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage)?;
        rows.map(|row| {
            let (key, json) = row.map_err(storage)?;
            Ok((key, serde_json::from_str(&json).map_err(storage)?))
        })
        .collect()
    }

    pub(super) fn settings(&self, companion: &str) -> Result<Option<Settings>> {
        self.get(&format!("settings:{companion}"))
    }

    pub(super) fn put_settings(&self, companion: &str, value: &Settings) -> Result<()> {
        self.put(&format!("settings:{companion}"), value)
    }

    pub(super) fn state(&self, companion: &str) -> Result<w::State> {
        Ok(self.get(&format!("state:{companion}"))?.unwrap_or_default())
    }

    pub(super) fn work(&self, work_id: &str) -> Result<Option<Work>> {
        self.get(&format!("work:{work_id}"))
    }

    pub(super) fn put_work(&self, work: &Work) -> Result<()> {
        self.put(&format!("work:{}", work.info.work_id), work)
    }

    pub(super) fn works_for(&self, companion: &str) -> Result<Vec<w::WorkInfo>> {
        Ok(self
            .list::<Work>("work:")?
            .into_iter()
            .filter_map(|(_, work)| (work.info.companion_id == companion).then_some(work.info))
            .collect())
    }

    pub(super) fn question(&self, companion: &str, id: &str) -> Result<Option<w::PendingRequest>> {
        self.get(&format!("question:{companion}:{id}"))
    }

    pub(super) fn put_question(&self, companion: &str, request: &w::PendingRequest) -> Result<()> {
        self.put(
            &format!("question:{companion}:{}", request.request_id),
            request,
        )
    }

    pub(super) fn questions_for(&self, companion: &str) -> Result<Vec<w::PendingRequest>> {
        Ok(self
            .list::<w::PendingRequest>(&format!("question:{companion}:"))?
            .into_iter()
            .map(|(_, request)| request)
            .collect())
    }

    pub(super) fn receipt(&self, conversation: &str, key: &str) -> Result<Option<Receipt>> {
        self.get(&format!("receipt:{conversation}:{key}"))
    }

    pub(super) fn put_receipt(
        &self,
        conversation: &str,
        key: &str,
        receipt: &Receipt,
    ) -> Result<()> {
        self.put(&format!("receipt:{conversation}:{key}"), receipt)
    }
}
