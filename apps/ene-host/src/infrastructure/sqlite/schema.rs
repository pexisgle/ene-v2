use super::*;
use tokio_rusqlite::rusqlite::OptionalExtension;

/// Marker that distinguishes this consolidated baseline from the historical
/// migration chain, whose first version was also 1.
const BASELINE: &str = "2026-11-consolidated";

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
    let companion = Companion {
        id: Uuid::new_v4().to_string(),
        name: "ene".into(),
        body: Default::default(),
        mind: "日本語で、簡潔で親しみやすく応答する。".into(),
        model_assignments: ModelAssignments::default().into(),
        availability: ene_rpc::JsonDocument::from(crate::domain::rules::capability_document(Some(
            "model_unset",
        )))
        .into(),
        revision: 1,
        ..Default::default()
    };
    for (key, value) in [
        ("schema_baseline", BASELINE.into()),
        ("host_id", Uuid::new_v4().to_string()),
        ("environment_id", Uuid::new_v4().to_string()),
        ("principal", Uuid::new_v4().to_string()),
        (
            "settings",
            serde_json::to_string(&Settings {
                revision: 1,
                connections: vec![],
                ..Default::default()
            })?,
        ),
    ] {
        tx.execute("INSERT INTO metadata VALUES (?1,?2)", params![key, value])?;
    }
    tx.execute(
        "INSERT INTO companions VALUES (?1,?2)",
        params![companion.id, serde_json::to_string(&companion)?],
    )?;
    Ok(())
}
pub(super) fn initialize(db: &mut Connection) -> Result<()> {
    let migrations = migrations();
    match migrations.current_version(db)? {
        rusqlite_migration::SchemaVersion::Outside(version) => {
            return Err(Error::FutureSchema(version.get()));
        }
        // The historical chain also reported version 1 after its first step.
        // Only the consolidated baseline carries the marker.
        rusqlite_migration::SchemaVersion::Inside(_) => {
            let baseline: Option<String> = db
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_baseline'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if baseline.as_deref() != Some(BASELINE) {
                return Err(Error::IncompatibleSchema);
            }
        }
        rusqlite_migration::SchemaVersion::NoneSet => {}
    }
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    migrations.to_latest(db)?;
    let tx = db.transaction()?;
    let pending: Vec<(String, String, String)> = {
        // A chat receipt stays pending until the kernel's saved log proves the
        // input was accepted; the host resolves it on the next send.
        let mut q = tx.prepare(
            "SELECT principal,key,response FROM receipts WHERE status=0 AND target NOT LIKE '/v1/companions/%/messages'",
        )?;
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (principal, key, body) in pending {
        let previous: serde_json::Value = serde_json::from_str(&body)?;
        let result = json!(ene_rpc::ApiFailure {
            request_id: conversation::request_id(&previous)
                .unwrap_or_default()
                .into(),
            error: Failure {
                code: "unavailable".into(),
                message: "ホストが中断しました".into(),
                details: ene_rpc::JsonDocument {
                    json: json!({"reason":"host_interrupted"}).to_string(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }
            .into(),
            http_status: 503,
            ..Default::default()
        });
        tx.execute(
            "UPDATE receipts SET status=503,response=?3 WHERE principal=?1 AND key=?2",
            params![principal, key, result.to_string()],
        )?;
    }
    tx.commit().map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_fresh_database_gets_the_baseline_without_the_legacy_messages_table() {
        let mut db = Connection::open_in_memory().unwrap();
        initialize(&mut db).unwrap();
        assert_eq!(meta(&db, "schema_baseline").unwrap(), BASELINE);
        assert_eq!(count(&db, "SELECT count(*) FROM companions"), 1);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM sqlite_master WHERE name='messages'"
            ),
            0
        );
        // Reopening an initialized database is a no-op, not a reseed.
        initialize(&mut db).unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM companions"), 1);
    }

    #[test]
    fn a_database_from_the_historical_chain_is_rejected_instead_of_used() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); PRAGMA user_version=1;",
        )
        .unwrap();
        assert!(matches!(
            initialize(&mut db),
            Err(Error::IncompatibleSchema)
        ));
    }

    #[test]
    fn a_newer_schema_is_rejected() {
        let mut db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        assert!(matches!(initialize(&mut db), Err(Error::FutureSchema(2))));
    }
}
