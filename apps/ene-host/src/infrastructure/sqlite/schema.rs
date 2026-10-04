use super::*;

fn migrations() -> rusqlite_migration::Migrations<'static> {
    use rusqlite_migration::{HookError, M, Migrations};
    Migrations::new(vec![M::up_with_hook(
        include_str!("../../../migrations/001_initial.sql"),
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
pub(super) fn initialize(db: &mut Connection) -> Result<()> {
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
