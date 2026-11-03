use super::*;
mod generated;
fn migrations() -> rusqlite_migration::Migrations<'static> {
    use rusqlite_migration::{HookError, M, Migrations};
    Migrations::new(vec![
        M::up_with_hook(include_str!("../../../migrations/001_initial.sql"), |tx| {
            seed(tx).map_err(|error| match error {
                Error::Sql(error) => HookError::RusqliteError(error),
                error => HookError::Hook(error.to_string()),
            })
        }),
        M::up(include_str!("../../../migrations/002_companions.sql")),
        M::up(include_str!("../../../migrations/003_rpc_state.sql")),
        M::up(include_str!("../../../migrations/004_plugins.sql")),
        M::up_with_hook(
            include_str!("../../../migrations/005_generated_messages.sql"),
            |tx| {
                generated::migrate(tx).map_err(|error| match error {
                    Error::Sql(error) => HookError::RusqliteError(error),
                    error => HookError::Hook(error.to_string()),
                })
            },
        ),
        M::up(include_str!("../../../migrations/006_mind.sql")),
        M::up(include_str!("../../../migrations/007_memory_index.sql")),
        M::up(include_str!("../../../migrations/008_memory_revision.sql")),
        M::up(include_str!(
            "../../../migrations/009_consolidation_cursor_offset.sql"
        )),
    ])
}
fn seed(tx: &tokio_rusqlite::rusqlite::Transaction<'_>) -> Result<()> {
    let unavailable = |reason: &str| json!({"ready":false,"reasons":[{"code":reason,"message":"未設定または未対応です"}]});
    let companion = Companion {
        id: Uuid::new_v4().to_string(),
        name: "ene".into(),
        body: Default::default(),
        voice: Default::default(),
        mind: "日本語で、簡潔で親しみやすく応答する。".into(),
        model_assignments: ModelAssignments::default().into(),
        availability: ene_rpc::JsonDocument { json: json!({"chat":unavailable("model_unset"),"task":unavailable("unsupported_capability"),"speech":unavailable("unsupported_capability")}).to_string(), ..Default::default() }.into(),
        revision: 1,
     ..Default::default() };
    for (key, value) in [
        ("generated_messages_seed", "1".into()),
        ("host_id", Uuid::new_v4().to_string()),
        ("environment_id", Uuid::new_v4().to_string()),
        ("principal", Uuid::new_v4().to_string()),
        ("companion", serde_json::to_string(&companion)?),
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
mod companion_migration_tests {
    use super::*;
    #[test]
    fn migration_keeps_existing_companion_identity_and_storage_updates_are_id_scoped() {
        let mut db = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut db, 1).unwrap();
        let mut original: Companion =
            serde_json::from_str(&meta(&db, "companion").unwrap()).unwrap();
        migrations().to_version(&mut db, 4).unwrap();
        db.execute(
            "DELETE FROM metadata WHERE key='generated_messages_seed'",
            [],
        )
        .unwrap();
        original.body = ene_rpc::JsonDocument {
            json: json!({"model":"saved-body"}).to_string(),
            ..Default::default()
        }
        .into();
        original.voice = ene_rpc::JsonDocument {
            json: json!({"voice":"saved-voice"}).to_string(),
            ..Default::default()
        }
        .into();
        let mut old = serde_json::to_value(&original).unwrap();
        for field in ["body", "voice", "availability"] {
            old[field] = serde_json::from_str(old[field]["json"].as_str().unwrap()).unwrap();
        }
        db.execute(
            "UPDATE companions SET body=?1 WHERE id=?2",
            params![old.to_string(), original.id],
        )
        .unwrap();
        let options = json!({"region":"saved", "temperature":0.5});
        db.execute("UPDATE metadata SET value=?1 WHERE key='settings'", [json!({"revision":7,"connections":[{"id":"saved-connection","kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":options,"credential_state":"registered","capabilities":{"streaming_text":true},"last_check":null}]}).to_string()]).unwrap();
        db.execute("INSERT INTO messages VALUES ('saved-message',?1,1,'failed',?2)", params![original.id,json!({"id":"saved-message","companion_id":original.id,"role":"companion","status":"failed","text":"saved text","sequence":1,"resources":[{"uri":"saved-resource"}],"failure":{"code":"saved-failure","message":"saved error","details":{"reason":"saved-reason"}}}).to_string()]).unwrap();
        db.execute("INSERT INTO plugin_records VALUES ('bindings','saved-binding',?1)", [json!({"id":"saved-binding","plugin_id":"reference","entrypoint":"behavior","revision":"saved-revision","companion_id":original.id,"configuration":{"mode":"saved"}}).to_string()]).unwrap();
        db.execute("INSERT INTO receipts VALUES ('saved-principal','saved-key','POST','saved-target','{}',503,?1)", [json!({"request_id":"saved-request","error":{"code":"saved-failure","message":"saved error","details":{"reason":"saved-reason"}}}).to_string()]).unwrap();
        migrations().to_latest(&mut db).unwrap();
        let mut config = conversation::load(&db).unwrap();
        assert_eq!(config.companions[&original.id], original);
        assert_eq!(config.settings.revision, 7);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&config.settings.connections[0].options.json)
                .unwrap(),
            options
        );
        // The legacy messages table is kept by the migrations and is not
        // rewritten; new conversations are saved by Rig.
        let saved: ene_rpc::Message = serde_json::from_str(
            &db.query_row(
                "SELECT body FROM messages WHERE companion_id=?1",
                [&original.id],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(saved.text, "saved text");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved.resources[0].json).unwrap(),
            json!({"uri":"saved-resource"})
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved.failure.details.json).unwrap(),
            json!({"reason":"saved-reason"})
        );
        let binding: ene_rpc::PluginBinding = serde_json::from_str(
            &db.query_row(
                "SELECT value FROM plugin_records WHERE namespace='bindings'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&binding.configuration.json).unwrap(),
            json!({"mode":"saved"})
        );
        let receipt: ene_rpc::ApiFailure = serde_json::from_str(
            &db.query_row(
                "SELECT response FROM receipts WHERE key='saved-key'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt.request_id, "saved-request");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&receipt.error.details.json).unwrap(),
            json!({"reason":"saved-reason"})
        );
        let mut other = original.clone();
        other.id = "other".into();
        other.mind = "separate mind".into();
        config.companions.insert(other.id.clone(), other.clone());
        conversation::write_configuration(&db, &config).unwrap();
        config.companions.get_mut(&original.id).unwrap().mind = "updated mind".into();
        conversation::write_configuration(&db, &config).unwrap();
        let loaded = conversation::load(&db).unwrap();
        assert_eq!(loaded.companions["other"], other);
        assert_eq!(loaded.companions[&original.id].mind, "updated mind");
        assert!(meta(&db, "companion").is_err());
    }
}
