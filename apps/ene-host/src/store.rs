use ene_protocol::host::{
    Companion, Failure, Message, MessageStatus, ModelAssignments, Page, Settings,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone)]
pub struct Store(mpsc::Sender<Command>);

pub struct StoredSnapshot {
    pub host_id: String,
    pub environment_id: String,
    pub companion: Companion,
    pub settings: Settings,
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

enum Command {
    Snapshot(oneshot::Sender<Result<StoredSnapshot>>),
    Messages(
        String,
        Option<String>,
        usize,
        oneshot::Sender<Result<Page<Message>>>,
    ),
    Message(String, String, oneshot::Sender<Result<Option<Message>>>),
}

impl Store {
    pub async fn open(path: PathBuf) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel(64);
        let (ready, wait) = oneshot::channel();
        std::thread::Builder::new()
            .name("ene-database".into())
            .spawn(move || {
                let db = Connection::open(path)
                    .map_err(|e| e.to_string())
                    .and_then(|mut db| {
                        initialize(&mut db)?;
                        Ok(db)
                    });
                let mut db = match db {
                    Ok(db) => {
                        let _ = ready.send(Ok(()));
                        db
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                while let Some(command) = rx.blocking_recv() {
                    match command {
                        Command::Snapshot(reply) => {
                            let _ = reply.send(snapshot(&mut db));
                        }
                        Command::Messages(c, cursor, limit, reply) => {
                            let _ = reply.send(page(&db, &c, cursor.as_deref(), limit));
                        }
                        Command::Message(c, id, reply) => {
                            let result = db
                                .query_row(
                                    "SELECT body FROM messages WHERE companion_id=?1 AND id=?2",
                                    params![c, id],
                                    |r| r.get::<_, String>(0),
                                )
                                .optional()
                                .map_err(|e| e.to_string())
                                .and_then(|s| {
                                    s.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
                                        .transpose()
                                });
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        wait.await
            .map_err(|_| "database startup failed".to_string())??;
        Ok(Self(tx))
    }

    pub async fn snapshot(&self) -> Result<StoredSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Command::Snapshot(tx))
            .await
            .map_err(|_| "database stopped".to_string())?;
        rx.await.map_err(|_| "database stopped".to_string())?
    }

    pub async fn messages(
        &self,
        c: String,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Message>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Command::Messages(c, cursor, limit, tx))
            .await
            .map_err(|_| "database stopped".to_string())?;
        rx.await.map_err(|_| "database stopped".to_string())?
    }

    pub async fn message(&self, c: String, id: String) -> Result<Option<Message>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Command::Message(c, id, tx))
            .await
            .map_err(|_| "database stopped".to_string())?;
        rx.await.map_err(|_| "database stopped".to_string())?
    }
}

fn initialize(db: &mut Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(|e| e.to_string())?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version > 1 {
        return Err("database schema is newer than this host".into());
    }
    if version == 0 {
        tx.execute_batch(include_str!("../migrations/001_initial.sql"))
            .map_err(|e| e.to_string())?;
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
            (
                "companion",
                serde_json::to_string(&companion).map_err(|e| e.to_string())?,
            ),
            (
                "settings",
                serde_json::to_string(&Settings {
                    revision: 1,
                    connections: vec![],
                })
                .map_err(|e| e.to_string())?,
            ),
        ] {
            tx.execute("INSERT INTO metadata VALUES (?1,?2)", params![key, value])
                .map_err(|e| e.to_string())?;
        }
        tx.execute_batch("PRAGMA user_version=1;")
            .map_err(|e| e.to_string())?;
    }
    let interrupted: Vec<(String, String)> = {
        let mut statement = tx
            .prepare("SELECT id,body FROM messages WHERE status IN ('queued','streaming')")
            .map_err(|e| e.to_string())?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| e.to_string())?
    };
    for (id, body) in interrupted {
        let mut message: Message = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        message.status = MessageStatus::Failed;
        message.failure = Some(Failure {
            code: "host_interrupted".into(),
            message: "ホストの停止により生成が中断されました。自動再実行しません。".into(),
            details: json!({}),
        });
        tx.execute(
            "UPDATE messages SET status='failed',body=?2 WHERE id=?1",
            params![
                id,
                serde_json::to_string(&message).map_err(|e| e.to_string())?
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn meta(db: &Connection, key: &str) -> Result<String> {
    db.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .map_err(|e| e.to_string())
}

pub fn parse_cursor(companion: &str, cursor: &str) -> Result<u64> {
    let (sequence, id) = cursor.split_once(':').ok_or("invalid_cursor")?;
    let sequence: u64 = sequence.parse().map_err(|_| "invalid_cursor")?;
    if id != companion || sequence == 0 || sequence > i64::MAX as u64 {
        return Err("invalid_cursor".into());
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
    let mut query=db.prepare("SELECT body FROM messages WHERE companion_id=?1 AND sequence<?2 ORDER BY sequence DESC LIMIT ?3").map_err(|e|e.to_string())?;
    let bodies = query
        .query_map(params![companion, before as i64, (limit + 1) as i64], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| e.to_string())?;
    let mut items: Vec<Message> = bodies
        .map(|body| {
            body.map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
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
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let companion: Companion =
        serde_json::from_str(&meta(&tx, "companion")?).map_err(|e| e.to_string())?;
    let recent = page(&tx, &companion.id, None, 50)?;
    let mut messages: BTreeMap<u64, Message> =
        recent.items.into_iter().map(|m| (m.sequence, m)).collect();
    {
        let mut query=tx.prepare("SELECT body FROM messages WHERE companion_id=?1 AND status IN ('queued','streaming')").map_err(|e|e.to_string())?;
        for body in query
            .query_map([&companion.id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
        {
            let m: Message = serde_json::from_str(&body.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            messages.insert(m.sequence, m);
        }
    }
    let result = StoredSnapshot {
        host_id: meta(&tx, "host_id")?,
        environment_id: meta(&tx, "environment_id")?,
        companion,
        settings: serde_json::from_str(&meta(&tx, "settings")?).map_err(|e| e.to_string())?,
        messages: messages.into_values().collect(),
        next_cursor: recent.next_cursor,
    };
    tx.commit().map_err(|e| e.to_string())?;
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
            created_at: chrono::Utc::now().to_rfc3339(),
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
        assert!(initialize(&mut db).is_err());
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }
}
