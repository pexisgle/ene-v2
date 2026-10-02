use ene_protocol::host::{
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
    #[error("database schema {0} is newer than this host")]
    FutureSchema(usize),
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Store {
    db: AsyncConnection,
    slots: Arc<Semaphore>,
}

pub struct StoredSnapshot {
    pub host_id: String,
    pub environment_id: String,
    pub companion: Companion,
    pub settings: Settings,
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

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

    pub async fn companion(&self, id: &str) -> Result<Option<Companion>> {
        let companion = self.snapshot().await?.companion;
        Ok((companion.id == id).then_some(companion))
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

fn migrations() -> rusqlite_migration::Migrations<'static> {
    use rusqlite_migration::{HookError, M, Migrations};
    Migrations::new(vec![M::up_with_hook(
        include_str!("../migrations/001_initial.sql"),
        |tx| {
            seed(tx).map_err(|error| match error {
                Error::Sql(error) => HookError::RusqliteError(error),
                error => HookError::Hook(error.to_string()),
            })
        },
    )])
}
fn seed(tx: &tokio_rusqlite::rusqlite::Transaction<'_>) -> Result<()> {
    let unavailable = |reason: &str| json!({"ready":false,"reasons":[{"code":reason,"message":"未設定または未対応です"}]});
    let companion = Companion {
        id: Uuid::new_v4().to_string(),
        name: "ene".into(),
        body: None,
        voice: None,
        mind: "日本語で、簡潔で親しみやすく応答する。".into(),
        model_assignments: ModelAssignments::default(),
        availability: json!({"chat":unavailable("model_unset"),"task":unavailable("unsupported_capability"),"speech":unavailable("unsupported_capability")}),
        revision: 1,
    };
    for (key, value) in [
        ("host_id", Uuid::new_v4().to_string()),
        ("environment_id", Uuid::new_v4().to_string()),
        ("principal", Uuid::new_v4().to_string()),
        ("companion", serde_json::to_string(&companion)?),
        (
            "settings",
            serde_json::to_string(&Settings {
                revision: 1,
                connections: vec![],
            })?,
        ),
    ] {
        tx.execute("INSERT INTO metadata VALUES (?1,?2)", params![key, value])?;
    }
    Ok(())
}
fn initialize(db: &mut Connection) -> Result<()> {
    let migrations = migrations();
    if let rusqlite_migration::SchemaVersion::Outside(version) = migrations.current_version(db)? {
        return Err(Error::FutureSchema(version.get()));
    }
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    migrations.to_latest(db)?;
    let tx = db.transaction()?;
    let interrupted: Vec<(String, String)> = {
        let mut statement =
            tx.prepare("SELECT id,body FROM messages WHERE status IN ('queued','streaming')")?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (id, body) in interrupted {
        let mut message: Message = serde_json::from_str(&body)?;
        message.status = MessageStatus::Failed;
        message.failure = Some(Failure {
            code: "host_interrupted".into(),
            message: "ホストの停止により生成が中断されました。自動再実行しません。".into(),
            details: json!({}),
        });
        tx.execute(
            "UPDATE messages SET status='failed',body=?2 WHERE id=?1",
            params![id, serde_json::to_string(&message)?],
        )?;
    }
    tx.commit().map_err(Error::from)
}

fn meta(db: &Connection, key: &str) -> Result<String> {
    db.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .map_err(Error::from)
}

pub fn parse_cursor(companion: &str, cursor: &str) -> Result<u64> {
    let (sequence, id) = cursor.split_once(':').ok_or(Error::InvalidCursor)?;
    let sequence: u64 = sequence.parse().map_err(|_| Error::InvalidCursor)?;
    if id != companion || sequence == 0 || sequence > i64::MAX as u64 {
        return Err(Error::InvalidCursor);
    }
    Ok(sequence)
}

fn page(
    db: &Connection,
    companion: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page<Message>> {
    let before = cursor
        .map(|c| parse_cursor(companion, c))
        .transpose()?
        .unwrap_or(i64::MAX as u64);
    let mut query=db.prepare("SELECT body FROM messages WHERE companion_id=?1 AND sequence<?2 ORDER BY sequence DESC LIMIT ?3")?;
    let bodies = query.query_map(params![companion, before as i64, (limit + 1) as i64], |r| {
        r.get::<_, String>(0)
    })?;
    let mut items: Vec<Message> = bodies
        .map(|body| {
            body.map_err(Error::from)
                .and_then(|s| serde_json::from_str(&s).map_err(Error::from))
        })
        .collect::<Result<_>>()?;
    let more = items.len() > limit;
    items.truncate(limit);
    items.reverse();
    let next_cursor = if more {
        items
            .first()
            .map(|m| format!("{}:{}", m.sequence, companion))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

fn snapshot(db: &mut Connection) -> Result<StoredSnapshot> {
    let tx = db.transaction()?;
    let companion: Companion = serde_json::from_str(&meta(&tx, "companion")?)?;
    let recent = page(&tx, &companion.id, None, 50)?;
    let mut messages: BTreeMap<u64, Message> =
        recent.items.into_iter().map(|m| (m.sequence, m)).collect();
    {
        let mut query = tx.prepare(
            "SELECT body FROM messages WHERE companion_id=?1 AND status IN ('queued','streaming')",
        )?;
        for body in query.query_map([&companion.id], |r| r.get::<_, String>(0))? {
            let m: Message = serde_json::from_str(&body?)?;
            messages.insert(m.sequence, m);
        }
    }
    let result = StoredSnapshot {
        host_id: meta(&tx, "host_id")?,
        environment_id: meta(&tx, "environment_id")?,
        companion,
        settings: serde_json::from_str(&meta(&tx, "settings")?)?,
        messages: messages.into_values().collect(),
        next_cursor: recent.next_cursor,
    };
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_protocol::host::Role;

    #[test]
    fn initialization_is_stable_and_restart_preserves_partial_failure() {
        let mut db = Connection::open_in_memory().unwrap();
        initialize(&mut db).unwrap();
        let first = snapshot(&mut db).unwrap();
        let principal = meta(&db, "principal").unwrap();
        let m = Message {
            id: "partial".into(),
            companion_id: first.companion.id.clone(),
            role: Role::Companion,
            text: "生成途中🙂".into(),
            status: MessageStatus::Streaming,
            created_at: "2026-10-01T00:00:00Z".into(),
            sequence: 1,
            resources: vec![],
            in_reply_to: None,
            task_id: None,
            question_id: None,
            failure: None,
        };
        db.execute(
            "INSERT INTO messages VALUES (?1,?2,1,'streaming',?3)",
            params![m.id, m.companion_id, serde_json::to_string(&m).unwrap()],
        )
        .unwrap();
        initialize(&mut db).unwrap();
        let second = snapshot(&mut db).unwrap();
        assert_eq!(first.host_id, second.host_id);
        assert_eq!(first.environment_id, second.environment_id);
        assert_eq!(first.companion, second.companion);
        assert_eq!(principal, meta(&db, "principal").unwrap());
        assert_eq!(second.messages[0].text, m.text);
        assert_eq!(second.messages[0].status, MessageStatus::Failed);
        assert_eq!(
            second.messages[0].failure.as_ref().unwrap().code,
            "host_interrupted"
        );
    }

    #[test]
    fn pagination_is_scoped_ordered_and_keeps_old_active_messages() {
        let mut db = Connection::open_in_memory().unwrap();
        initialize(&mut db).unwrap();
        let c = snapshot(&mut db).unwrap().companion.id;
        for n in 1..=61 {
            let status = if n == 1 {
                MessageStatus::Streaming
            } else {
                MessageStatus::Completed
            };
            let m = Message {
                id: format!("m{n}"),
                companion_id: c.clone(),
                role: Role::Companion,
                text: format!("メモ{n}"),
                status: status.clone(),
                created_at: "2026-10-01T00:00:00Z".into(),
                sequence: n,
                resources: vec![],
                in_reply_to: None,
                task_id: None,
                question_id: None,
                failure: None,
            };
            db.execute(
                "INSERT INTO messages VALUES (?1,?2,?3,?4,?5)",
                params![
                    m.id,
                    c,
                    n as i64,
                    if status == MessageStatus::Streaming {
                        "streaming"
                    } else {
                        "completed"
                    },
                    serde_json::to_string(&m).unwrap()
                ],
            )
            .unwrap();
        }
        let p = page(&db, &c, None, 50).unwrap();
        assert_eq!(p.items.first().unwrap().sequence, 12);
        assert_eq!(p.items.last().unwrap().sequence, 61);
        let older = page(&db, &c, p.next_cursor.as_deref(), 50).unwrap();
        assert_eq!(older.items.len(), 11);
        assert!(older.next_cursor.is_none());
        assert!(page(&db, "other", p.next_cursor.as_deref(), 50).is_err());
        let s = snapshot(&mut db).unwrap();
        assert_eq!(s.messages.len(), 51);
        assert_eq!(s.messages[0].sequence, 1);
    }

    #[test]
    fn future_schema_is_rejected_without_rewriting() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version=99;").unwrap();
        assert!(matches!(initialize(&mut db), Err(Error::FutureSchema(99))));
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn seed_failure_rolls_back_schema_data_and_version() {
        use rusqlite_migration::{HookError, M, Migrations};
        let migrations = Migrations::new(vec![M::up_with_hook(
            include_str!("../migrations/001_initial.sql"),
            |tx| {
                seed(tx).unwrap();
                Err(HookError::Hook("injected failure after seed".into()))
            },
        )]);
        let mut db = Connection::open_in_memory().unwrap();
        assert!(migrations.to_latest(&mut db).is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        initialize(&mut db).unwrap();
        assert!(!snapshot(&mut db).unwrap().host_id.is_empty());
    }
    #[test]
    fn rejecting_a_future_database_leaves_its_bytes_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("future.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE future(value TEXT); INSERT INTO future VALUES ('keep'); PRAGMA user_version=99;").unwrap();
        db.close().unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut db = Connection::open(&path).unwrap();
        assert!(matches!(initialize(&mut db), Err(Error::FutureSchema(99))));
        db.close().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
    #[tokio::test]
    async fn caller_cancellation_keeps_the_database_slot_until_sql_finishes() {
        let db = AsyncConnection::open_in_memory().await.unwrap();
        let store = Store {
            db,
            slots: Arc::new(Semaphore::new(1)),
        };
        store.call(initialize).await.unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let clone = store.clone();
        let operation = tokio::spawn(async move {
            clone
                .call(move |_| {
                    let _ = started.send(());
                    wait.recv().unwrap();
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        operation.abort();
        let _ = operation.await;
        assert_eq!(store.slots.available_permits(), 0);
        release.send(()).unwrap();
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(1), store.snapshot())
                .await
                .unwrap()
                .unwrap()
                .host_id
                .is_empty()
        );
        store.db.clone().close().await.unwrap();
        assert!(matches!(store.snapshot().await, Err(Error::Stopped)));
        assert!(matches!(
            parse_cursor("c", "bad"),
            Err(Error::InvalidCursor)
        ));
    }
}
