//! SQLite execution, schema lifecycle and repository implementation.
mod queries;
mod repository;
mod schema;
#[cfg(test)]
mod tests;
#[cfg(test)]
use queries::meta;
use queries::{page, snapshot};
use schema::initialize;

pub(crate) mod conversation;
use ene_core::domain::{
    Companion, Failure, Message, MessageStatus, ModelAssignments, Page, Settings,
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;
use tokio_rusqlite::{
    Connection as AsyncConnection,
    rusqlite::{Connection, OptionalExtension, params},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SQLite: {0}")]
    Sql(#[from] tokio_rusqlite::rusqlite::Error),
    #[error("stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("schema migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("database stopped")]
    Stopped,
    #[error("invalid_cursor")]
    InvalidCursor,
    #[error("stored message exceeds history page budget")]
    OversizedMessage,
    #[error("snapshot exceeds response budget")]
    ResourceLimit,
    #[error("invalid stored receipt")]
    CorruptReceipt,
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    #[error("state_conflict")]
    StateConflict,
    #[error("database schema {0} is newer than this host")]
    FutureSchema(usize),
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Store {
    db: AsyncConnection,
    slots: Arc<Semaphore>,
}

pub use ene_core::domain::StoredSnapshot;

impl Store {
    pub async fn open(path: PathBuf) -> Result<Self> {
        let store = Self {
            db: AsyncConnection::open(path).await.map_err(Error::Sql)?,
            // Bound the library's unbounded queue, including the running operation.
            slots: Arc::new(Semaphore::new(64)),
        };
        store.call(initialize).await?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.call(operation).await
    }

    async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Stopped)?;
        self.db
            .call(move |db| {
                // Keep the slot until SQL finishes even if the caller is cancelled.
                let _permit = permit;
                operation(db)
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(error) => error,
                tokio_rusqlite::Error::Close((_, error)) => Error::Sql(error),
                _ => Error::Stopped,
            })
    }

    pub async fn snapshot(&self) -> Result<StoredSnapshot> {
        self.call(snapshot).await
    }

    pub async fn messages(
        &self,
        c: String,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Message>> {
        self.call(move |db| page(db, &c, cursor.as_deref(), limit))
            .await
    }

    pub async fn message(&self, c: String, id: String) -> Result<Option<Message>> {
        self.call(move |db| {
            db.query_row(
                "SELECT body FROM messages WHERE companion_id=?1 AND id=?2",
                params![c, id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Error::from)
            .and_then(|s| {
                s.map(|s| serde_json::from_str(&s).map_err(Error::from))
                    .transpose()
            })
        })
        .await
    }
}

impl Store {
    pub async fn rpc_state(&self) -> Result<Option<Vec<u8>>> {
        self.call(|db| {
            Ok(db
                .query_row("SELECT body FROM rpc_state WHERE key='devices'", [], |r| {
                    r.get(0)
                })
                .optional()?)
        })
        .await
    }
    pub async fn save_rpc_state(&self, bytes: Vec<u8>) -> Result<()> {
        self.call(move |db| { db.execute("INSERT INTO rpc_state VALUES ('devices',?1) ON CONFLICT(key) DO UPDATE SET body=excluded.body", params![bytes])?; Ok(()) }).await
    }
}
