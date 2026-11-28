use crate::error::{conflict, invalid};
use anyhow::Result;
use ene_rpc::{AuditEntry, Mutation};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{
    FromRow, Sqlite, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
    types::Json,
};
use std::{path::Path, time::Duration};

pub(crate) async fn open(directory: &Path) -> Result<SqlitePool> {
    let pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(directory.join("ene-v2.sqlite"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .pragma("secure_delete", "ON")
            .busy_timeout(Duration::from_secs(5)),
    )
    .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

pub(crate) async fn get<T: DeserializeOwned + Send + Unpin + 'static>(
    db: &mut SqliteConnection,
    kind: &str,
    id: &str,
) -> Result<Option<T>> {
    Ok(
        sqlx::query_scalar::<Sqlite, Json<T>>("SELECT body FROM records WHERE kind=? AND id=?")
            .bind(kind)
            .bind(id)
            .fetch_optional(db)
            .await?
            .map(|row| row.0),
    )
}

pub(crate) async fn list<T: DeserializeOwned + Send + Unpin + 'static>(
    db: &mut SqliteConnection,
    kind: &str,
) -> Result<Vec<T>> {
    Ok(
        sqlx::query_scalar::<Sqlite, Json<T>>("SELECT body FROM records WHERE kind=? ORDER BY id")
            .bind(kind)
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|row| row.0)
            .collect(),
    )
}

pub(crate) async fn put<T: Serialize + Sync>(
    db: &mut SqliteConnection,
    kind: &str,
    id: &str,
    expected: i64,
    value: &T,
) -> Result<()> {
    let changes = if expected == 0 {
        sqlx::query("INSERT OR IGNORE INTO records(kind,id,revision,body) VALUES(?,?,1,?)")
            .bind(kind)
            .bind(id)
            .bind(Json(value))
            .execute(db)
            .await?
    } else {
        sqlx::query(
            "UPDATE records SET body=?,revision=revision+1 WHERE kind=? AND id=? AND revision=?",
        )
        .bind(Json(value))
        .bind(kind)
        .bind(id)
        .bind(expected)
        .execute(db)
        .await?
    };
    if changes.rows_affected() == 0 {
        return Err(conflict("record changed since it was read"));
    }
    Ok(())
}

pub(crate) async fn delete(
    db: &mut SqliteConnection,
    kind: &str,
    id: &str,
    expected: i64,
) -> Result<()> {
    let result = sqlx::query("DELETE FROM records WHERE kind=? AND id=? AND revision=?")
        .bind(kind)
        .bind(id)
        .bind(expected)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(conflict("record changed since it was read"));
    }
    Ok(())
}

pub(crate) fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        serde_json_canonicalizer::to_vec(value)?,
    )))
}

pub(crate) fn key(mutation: Option<&Mutation>) -> Result<&Mutation> {
    mutation
        .filter(|value| {
            !value.request_key.is_empty()
                && value.request_key.len() <= 128
                && (0..i64::MAX).contains(&value.revision)
        })
        .ok_or_else(|| invalid("request key required"))
}

pub(crate) async fn replay<T: DeserializeOwned + Send + Unpin + 'static>(
    db: &mut SqliteConnection,
    key: &str,
    fingerprint: &str,
) -> Result<Option<T>> {
    let row = sqlx::query_as::<Sqlite, (String, String)>(
        "SELECT fingerprint, response FROM receipts WHERE key=?",
    )
    .bind(key)
    .fetch_optional(db)
    .await?;
    match row {
        Some((hash, _)) if hash != fingerprint => Err(conflict("request key already used")),
        Some((_, value)) => Ok(Some(serde_json::from_str(&value)?)),
        None => Ok(None),
    }
}

pub(crate) async fn acknowledge(
    db: &mut SqliteConnection,
    key: &str,
    fingerprint: &str,
    response: &(impl Serialize + Sync),
) -> Result<()> {
    sqlx::query("INSERT INTO receipts(key,fingerprint,response) VALUES(?,?,?)")
        .bind(key)
        .bind(fingerprint)
        .bind(Json(response))
        .execute(db)
        .await?;
    Ok(())
}

#[derive(FromRow)]
struct AuditRow {
    position: i64,
    time: i64,
    operation: String,
    decision: String,
    source: String,
}

pub(crate) async fn audit_history(
    db: &SqlitePool,
    before: u64,
    limit: u32,
) -> Result<Vec<AuditEntry>> {
    let before = if before == 0 {
        i64::MAX
    } else {
        i64::try_from(before)?
    };
    Ok(sqlx::query_as::<_, AuditRow>(
        "SELECT position,time,operation,decision,source FROM audit WHERE position<? ORDER BY position DESC LIMIT ?",
    )
    .bind(before)
    .bind(limit.clamp(1, 100))
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| AuditEntry {
        position: row.position.cast_unsigned(),
        time: row.time,
        operation: row.operation,
        decision: row.decision,
        source: row.source,
        ..Default::default()
    })
    .collect())
}

pub(crate) async fn audit(
    db: &mut SqliteConnection,
    operation: &str,
    decision: &str,
    source: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO audit(time,operation,decision,source) VALUES(unixepoch(),?,?,?)")
        .bind(operation)
        .bind(decision)
        .bind(source)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM audit WHERE time < unixepoch()-7776000 OR position <= (SELECT max(position)-10000 FROM audit)")
        .execute(db)
        .await?;
    Ok(())
}
