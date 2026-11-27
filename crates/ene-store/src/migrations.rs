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
pub const VERSION: i64 = 16;

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
    (
        "0009_activation_companion_fk",
        "CREATE TABLE activations_v9 (
            companion_id TEXT NOT NULL PRIMARY KEY REFERENCES companions(id) ON DELETE CASCADE,
            client_id TEXT,
            updated_at TEXT NOT NULL
        );
        INSERT INTO activations_v9 (companion_id, client_id, updated_at)
            SELECT a.companion_id, a.client_id, a.updated_at
            FROM activations a JOIN companions c ON c.id = a.companion_id;
        DROP TABLE activations;
        ALTER TABLE activations_v9 RENAME TO activations;",
    ),
    (
        "0010_approval_external_destination",
        "ALTER TABLE approval_requests ADD COLUMN external_destination TEXT;",
    ),
    (
        "0011_derive_client_grants",
        "ALTER TABLE clients DROP COLUMN granted_capabilities;",
    ),
    (
        "0012_device_operation_scope",
        "ALTER TABLE device_operations ADD COLUMN companion_id TEXT;
        ALTER TABLE device_operations ADD COLUMN run_id TEXT;
        UPDATE device_operations
            SET status = 'failed',
                failure = json_object('code', 'operation_owner_unknown', 'message',
                    'the queued operation has no recorded owner and was not dispatched'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            WHERE status = 'pending';
        CREATE INDEX idx_device_pending_run ON device_operations(run_id) WHERE status = 'pending';",
    ),
    (
        "0013_connection_incarnation",
        "ALTER TABLE connections ADD COLUMN created_revision INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        "0014_derive_work_approval_wait",
        "UPDATE work_runs SET state = 'running' WHERE state = 'waiting_input';
        ALTER TABLE work_runs DROP COLUMN pending_request_id;
        CREATE INDEX idx_pending_approvals_work_run
          ON approval_requests(work_id, run_id, created_at, request_id) WHERE status = 'pending';
        CREATE VIEW work_run_view AS
          SELECT r.run_id, r.work_id, r.instruction,
                 CASE WHEN a.request_id IS NOT NULL THEN 'waiting_input' ELSE r.state END AS state,
                 a.request_id AS pending_request_id, r.failure, r.started_at, r.updated_at
            FROM work_runs r LEFT JOIN approval_requests a ON a.request_id = (
              SELECT request_id FROM approval_requests p
                WHERE r.state IN ('starting','running') AND p.status = 'pending'
                  AND p.work_id = r.work_id AND p.run_id = r.run_id
                ORDER BY p.created_at, p.request_id LIMIT 1
          );",
    ),
    (
        "0015_record_work_executor",
        r#"ALTER TABLE work_runs ADD COLUMN executor TEXT NOT NULL DEFAULT '{"type":"unrecorded"}';"#,
    ),
    (
        "0016_pin_plugin_package_revision",
        "ALTER TABLE plugin_entries ADD COLUMN revision TEXT;",
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
#[path = "migrations_tests.rs"]
mod tests;
