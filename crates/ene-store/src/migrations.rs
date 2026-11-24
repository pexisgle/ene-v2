//! Forward-only SQLite schema initialization and incremental migrations.
//!
//! [`Migrations`] owns the forward ladder. It reads `PRAGMA user_version`, runs
//! only the pending steps, wraps a run in a transaction, and rolls that
//! transaction back when a step fails. Three things stay in this module because
//! the crate has no concept of them.
//!
//! - The baseline is a schema *file* rather than a step, so `schema.sql` is step
//!   0 and the crate's own accounting stamps version 6 once the padding steps
//!   after it have run.
//! - The `schema_migrations` audit rows are host bookkeeping, written from an
//!   `up` hook so each lands in the same transaction as the step it describes.
//! - The pre-baseline refusal is a safety rule about versions *below* the
//!   ladder, so it runs before the crate is allowed to touch the file.
//!
//! The crate names a version after the number of steps applied, so step `n`
//! (0-based) produces version `n + 1` and version `v` lives at index `v - 1`.
//! That is why steps 2..=6 exist but are empty. `schema.sql` already carries the
//! tables those versions describe, yet the crate needs one entry per version for
//! its index arithmetic to keep matching `user_version`.

use crate::{StoreError, error::Result, util::now};
use rusqlite::{Connection, Transaction, params};
use rusqlite_migration::{HookResult, M, Migrations};

/// Baseline schema version for `schema.sql`. Databases older than this baseline
/// (version 1..=5, or unversioned non-empty version 0 databases) are refused.
pub const BASELINE_VERSION: i64 = 6;

/// Latest database schema version managed by [`ladder_steps`].
pub const VERSION: i64 = 8;

/// Every version this host applies above the baseline, in order. The position
/// in this table is the version, so adding a row is the only way to add a
/// version and the step and its audit name cannot drift apart.
const STEPS_ABOVE_BASELINE: &[(&str, &str)] = &[
    (
        "0007_schema_migrations_table",
        "CREATE TABLE IF NOT EXISTS schema_migrations (
        version INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        applied_at TEXT NOT NULL
    );",
    ),
    (
        "0008_work_sessions_companion_index",
        "CREATE INDEX IF NOT EXISTS idx_work_sessions_companion
            ON work_sessions(companion_id, updated_at DESC, id);",
    ),
];

/// The version a step at `index` produces.
fn version_of(index: usize) -> i64 {
    BASELINE_VERSION + 1 + index as i64
}

/// The forward ladder, one entry per version from 1 to [`VERSION`].
///
/// Built per call rather than held in a `const` because the audited steps carry
/// hooks, which the crate only accepts at runtime.
fn ladder_steps() -> Vec<M<'static>> {
    let mut steps = Vec::with_capacity(VERSION as usize);
    // Step 0 carries `schema.sql`, and steps 1..=5 stay empty. The baseline is a
    // file, so versions 2..=6 have no DDL of their own, yet the crate counts
    // steps to name a version and needs one entry each.
    steps.push(M::up(include_str!("schema.sql")));
    steps.resize_with(BASELINE_VERSION as usize, || M::up(""));
    steps.extend(
        STEPS_ABOVE_BASELINE
            .iter()
            .enumerate()
            .map(|(index, (name, sql))| {
                M::up_with_hook(sql, move |tx| record(tx, version_of(index), name))
            }),
    );
    steps
}

/// Records one step in `schema_migrations`, inside that step's transaction.
fn record(tx: &Transaction<'_>, version: i64, name: &str) -> HookResult {
    tx.execute(
        "INSERT OR REPLACE INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![version, name, now()],
    )?;
    Ok(())
}

/// Verifies `PRAGMA user_version` before mutating connection pragmas (such as WAL).
pub(crate) fn validate_open_version(connection: &Connection) -> Result<i64> {
    validate_version_for(connection, VERSION)
}

fn validate_version_for(connection: &Connection, latest: i64) -> Result<i64> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > latest {
        return Err(StoreError::Storage(format!(
            "state database version {version} is newer than this host ({latest})"
        )));
    }
    if version < BASELINE_VERSION {
        // Only an empty unversioned database is initialized from baseline `schema.sql`.
        let tables: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if version > 0 || tables > 0 {
            return Err(StoreError::Storage(format!(
                "state database version {version} predates this host's baseline ({BASELINE_VERSION}); it is not migrated in place"
            )));
        }
    }
    Ok(version)
}

/// Initializes an empty DB from baseline v6 and applies all pending forward migrations.
pub(crate) fn apply_migrations(connection: &mut Connection) -> Result<()> {
    apply_ladder(connection, ladder_steps())
}

fn apply_ladder(connection: &mut Connection, steps: Vec<M<'static>>) -> Result<()> {
    let latest = steps.len() as i64;
    let migrations = Migrations::new(steps);
    let current = validate_version_for(connection, latest)?;
    let current = if current == 0 {
        // Versions 1..=6 are the baseline: `schema.sql` plus the empty padding
        // versions. One call, so the whole baseline commits or rolls back as a
        // unit and leaves `user_version` at `BASELINE_VERSION`.
        run(&migrations, connection, BASELINE_VERSION)?;
        BASELINE_VERSION
    } else {
        current
    };
    // Then one call per version rather than a single `to_latest`, so each
    // migration commits on its own and a late failure leaves `user_version` at
    // the last step that really succeeded instead of discarding the whole run.
    for version in current + 1..=latest {
        run(&migrations, connection, version)?;
    }
    Ok(())
}

fn run(migrations: &Migrations<'_>, connection: &mut Connection, version: i64) -> Result<()> {
    migrations
        .to_version(connection, version as usize)
        .map_err(|error| StoreError::Storage(error.to_string()))
}

#[cfg(test)]
mod tests {
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
            id: "c-v6".into(),
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
                conn_row.id,
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
        vec![(companion.id, companion.name), (conn_row.id, conn_row.name)]
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
        assert_eq!(user_version(&store.conn().unwrap()), VERSION);
        let settings = store.settings_repo().settings().unwrap();
        assert_eq!(settings.revision, 3);
        assert_eq!(settings.connections.len(), 1);
        assert_eq!(settings.connections[0].id, seeded[1].0);
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
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);

        let indexed: i64 = store
            .conn()
            .unwrap()
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
        assert_eq!(user_version(&reopened.conn().unwrap()), VERSION);
    }

    #[test]
    fn a_fresh_database_also_has_the_work_session_index() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let indexed: i64 = store
            .conn()
            .unwrap()
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
}
