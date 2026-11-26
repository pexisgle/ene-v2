use super::*;
use crate::Store;
use ene_domain::{
    Client, Connection as DomainConnection, ConnectionConfig, ConnectionKind, CredentialState,
    Endpoint, LlmConnection, WorkSession, WorkStatus,
};
use ene_mind::default_ene_companion;
use ene_policy::{PolicyEffect, PolicyRule};

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn seed_v6_database(connection: &mut Connection) -> Vec<(String, String)> {
    let tx = connection.transaction().unwrap();
    tx.execute_batch(include_str!("schema.sql")).unwrap();
    let stamp = "2026-01-01T00:00:00Z";
    let companion = default_ene_companion(stamp);
    tx.execute(
        "INSERT INTO companions (id, name, mind, profile, profile_revision, state, state_revision, \
         body, voice, assignments, persona, voice_identifier, revision, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            companion.id,
            companion.name,
            companion.mind,
            serde_json::to_string(&companion.profile).unwrap(),
            companion.profile_revision,
            serde_json::to_string(&companion.state).unwrap(),
            companion.state_revision,
            serde_json::to_string(&companion.body).unwrap(),
            serde_json::to_string(&companion.voice).unwrap(),
            serde_json::to_string(&companion.assignments).unwrap(),
            companion.persona,
            companion.voice_identifier,
            companion.revision,
            stamp,
            stamp,
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO settings_state (id, revision, updated_at) VALUES (1, 3, ?1)",
        [stamp],
    )
    .unwrap();
    let conn_row = DomainConnection {
        id: ene_domain::ConnectionId::parse("c-v6").unwrap(),
        name: "Legacy v6 OpenAI".into(),
        enabled: true,
        kind: ConnectionKind::Llm,
        config: ConnectionConfig::Llm(LlmConnection {
            endpoint: Endpoint {
                adapter: "openai".into(),
                endpoint: "https://api.openai.com/v1".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            model_ids: vec!["gpt-5".into()],
        }),
        credential_state: CredentialState::Registered,
        revision: 2,
        last_check: None,
    };
    tx.execute(
        "INSERT INTO connections (id, name, enabled, kind, config, credential_state, revision, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            conn_row.id.as_str(),
            conn_row.name,
            conn_row.enabled,
            conn_row.kind.as_str(),
            serde_json::to_string(&conn_row.config).unwrap(),
            conn_row.credential_state.as_str(),
            conn_row.revision,
            stamp,
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO policy_rules (id, tool_name, pattern, effect, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params!["rule-v6", "device_request", "*", "allow", stamp],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO clients (id, name, platform, capabilities, granted_capabilities, capability_specs, revision, last_seen) \
         VALUES ('client-v6', 'Desktop', 'windows', '[]', '[]', '[]', 1, ?1)",
        [stamp],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO work_sessions (id, companion_id, title, status, client_id, created_at, updated_at) \
         VALUES ('work-v6', ?1, 'v6 Task', 'active', 'client-v6', ?2, ?2)",
        params![companion.id, stamp],
    )
    .unwrap();
    tx.pragma_update(None, "user_version", BASELINE_VERSION)
        .unwrap();
    tx.commit().unwrap();
    vec![
        (companion.id, companion.name),
        (conn_row.id.into(), conn_row.name),
    ]
}

#[test]
fn migration_definitions_are_sequential_and_match_version() {
    // Replays every step against an in-memory database, so a malformed ladder
    // fails here in CI instead of at a user's startup.
    Migrations::new(ladder_steps()).validate().unwrap();
    assert_eq!(
        ladder_steps().len(),
        VERSION as usize,
        "the crate indexes a version by step count, so the ladder needs one \
         step per version or `user_version` stops matching the schema"
    );
    assert_eq!(
        BASELINE_VERSION + STEPS_ABOVE_BASELINE.len() as i64,
        VERSION,
        "every version above the baseline needs a step and an audit name"
    );
    for (name, sql) in STEPS_ABOVE_BASELINE {
        assert_ne!(name.trim().len(), 0);
        assert_ne!(sql.trim().len(), 0);
    }
}

#[test]
fn in_memory_fresh_db_applies_all_migrations_idempotently() {
    let mut conn = Connection::open_in_memory().unwrap();
    apply_migrations(&mut conn).unwrap();
    assert_eq!(user_version(&conn), VERSION);

    let recorded: (i64, String) = conn
        .query_row(
            "SELECT version, name FROM schema_migrations WHERE version = 7",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(recorded, (7, "0007_schema_migrations_table".to_owned()));

    for table in [
        "companions",
        "connections",
        "work_sessions",
        "plugin_entries",
        "schema_migrations",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "expected table {table} to exist");
    }

    // Re-running migrations on a current database is a clean no-op.
    apply_migrations(&mut conn).unwrap();
    assert_eq!(user_version(&conn), VERSION);
}

#[test]
fn upgrades_v6_database_in_place_preserving_existing_rows() {
    let mut mem = Connection::open_in_memory().unwrap();
    seed_v6_database(&mut mem);
    assert_eq!(user_version(&mem), BASELINE_VERSION);
    apply_migrations(&mut mem).unwrap();
    assert_eq!(user_version(&mem), VERSION);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host.sqlite3");
    let seeded = {
        let mut raw = Connection::open(&path).unwrap();
        seed_v6_database(&mut raw)
    };

    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&store.conn()), VERSION);
    let settings = store.settings_repo().settings().unwrap();
    assert_eq!(settings.revision, 3);
    assert_eq!(settings.connections.len(), 1);
    assert_eq!(settings.connections[0].id.as_str(), seeded[1].0);
    assert_eq!(settings.connections[0].name, seeded[1].1);

    let companions = store.companion_repo().companions().unwrap();
    assert_eq!(companions.len(), 1);
    assert_eq!(companions[0].id, seeded[0].0);

    let rules: Vec<PolicyRule> = store.policy_repo().policy_rules().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].effect, PolicyEffect::Allow);

    let client: Client = store.client_repo().client("client-v6").unwrap().unwrap();
    assert_eq!(client.name, "Desktop");

    let work: WorkSession = store.work_repo().work_session("work-v6").unwrap().unwrap();
    assert_eq!(work.status, WorkStatus::Active);

    let recorded: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 7",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1);

    let indexed: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
             AND name = 'idx_work_sessions_companion'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "the work-session index migration must run");

    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.settings_repo().settings().unwrap(), settings);
    assert_eq!(user_version(&reopened.conn()), VERSION);
}

#[test]
fn a_fresh_database_also_has_the_work_session_index() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let indexed: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
             AND name = 'idx_work_sessions_companion'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "baseline schema and migrations must agree");
}

#[test]
fn v8_activation_migration_preserves_live_rows_and_rejects_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host.sqlite3");
    let companion = {
        let mut conn = Connection::open(&path).unwrap();
        let companion = seed_v6_database(&mut conn).remove(0).0;
        let mut steps = ladder_steps();
        steps.truncate(8);
        apply_ladder(&mut conn, steps).unwrap();
        conn.execute(
            "INSERT INTO activations (companion_id, client_id, updated_at)
             VALUES (?1, 'client-v6', '2026-01-01'), ('deleted', 'client-v6', '2026-01-02')",
            [&companion],
        )
        .unwrap();
        companion
    };

    let store = Store::open(&path).unwrap();
    let repo = store.client_repo();
    let active = repo.activations().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].companion_id.as_str(), companion);
    assert_eq!(active[0].client_id.as_deref(), Some("client-v6"));
    let updated_at: String = store
        .conn()
        .query_row("SELECT updated_at FROM activations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(updated_at, "2026-01-01");
    assert!(repo.activate("deleted", Some("client-v6")).is_err());
    assert!(store.companion_repo().delete_companion(&companion).unwrap());
    assert_eq!(
        repo.activations().unwrap(),
        [] as [ene_domain::Activation; 0]
    );
}

#[test]
fn refuses_pre_baseline_versions_without_modifying_database() {
    let mut v0_populated = Connection::open_in_memory().unwrap();
    v0_populated
        .execute_batch("CREATE TABLE companions (id TEXT PRIMARY KEY)")
        .unwrap();
    assert!(matches!(
        validate_open_version(&v0_populated),
        Err(StoreError::Storage(_))
    ));
    assert!(matches!(
        apply_migrations(&mut v0_populated),
        Err(StoreError::Storage(_))
    ));
    assert_eq!(user_version(&v0_populated), 0);

    for pre_baseline in [1_i64, 5] {
        let mut old = Connection::open_in_memory().unwrap();
        old.pragma_update(None, "user_version", pre_baseline)
            .unwrap();
        assert!(matches!(
            validate_open_version(&old),
            Err(StoreError::Storage(_))
        ));
        assert!(matches!(
            apply_migrations(&mut old),
            Err(StoreError::Storage(_))
        ));
        assert_eq!(user_version(&old), pre_baseline);
    }
}

#[test]
fn failed_migration_rolls_back_ddl_and_user_version() {
    let mut conn = Connection::open_in_memory().unwrap();
    // A ladder that is well formed up to version 7 and broken at 8.
    let mut steps = ladder_steps();
    steps[VERSION as usize - 1] =
        M::up("CREATE TABLE should_roll_back (id INTEGER PRIMARY KEY); THIS IS INVALID SQL;");
    let result = apply_ladder(&mut conn, steps);
    assert!(matches!(result, Err(StoreError::Storage(_))));
    // The rollback must leave the last good version from this fixture, not
    // whatever the crate's global `VERSION` happens to be.
    assert_eq!(user_version(&conn), VERSION - 1);
    let leaked_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'should_roll_back'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked_table, 0);
}
