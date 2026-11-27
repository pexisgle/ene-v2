//! Work sessions and runs persistence.

use crate::row::{FromRow, row_fields};
use crate::{
    Store, StoreError,
    error::Result,
    json_column, require_found, to_json,
    util::{now, or_stamp},
};
use ene_domain::{Failure, Run, RunExecutor, RunState, WorkSession, WorkStatus};
use rusqlite::params;

row_fields!(WorkSession, SESSION_COLUMNS {
    id,
    companion_id => id_column("stored companion id is invalid"),
    title,
    status => enum_name_column("unknown work status"),
    client_id,
    created_at,
    updated_at,
});

row_fields!(Run, RUN_COLUMNS {
    run_id,
    work_id,
    instruction,
    state => enum_name_column("unknown run state"),
    pending_request_id,
    failure => optional_json_column("stored failure is corrupt"),
    started_at,
    updated_at,
});

/// A run is open while it can still produce events.
pub fn open_state(state: RunState) -> bool {
    matches!(
        state,
        RunState::Starting | RunState::Running | RunState::WaitingInput
    )
}

fn stored_run_state(state: RunState) -> &'static str {
    match state {
        RunState::WaitingInput => RunState::Running.as_str(),
        state => state.as_str(),
    }
}

/// Repository managing `work_sessions` and `work_runs`.
#[derive(Clone, Copy, Debug)]
pub struct WorkRepo<'a>(&'a Store);

/// The idempotency receipt of one accepted input.
#[derive(Clone, Copy, Debug)]
pub struct WorkInputReceipt<'a> {
    pub request_key: &'a str,
    pub payload_hash: &'a [u8],
    pub request_id: &'a str,
}

impl<'a> WorkRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn work_sessions(&self, companion: &str) -> Result<Vec<WorkSession>> {
        self.0.fetch_all(
            &format!(
                "SELECT {SESSION_COLUMNS} FROM work_sessions WHERE companion_id = ?1 \
                 ORDER BY updated_at DESC, id"
            ),
            [companion],
            WorkSession::from_row,
        )
    }

    /// Every work session, newest first per companion.
    ///
    /// One query rather than one per companion: a snapshot over a host with
    /// many companions otherwise pays a round trip for each.
    pub fn all_work_sessions(&self) -> Result<Vec<WorkSession>> {
        self.0.fetch_all(
            &format!(
                "SELECT {SESSION_COLUMNS} FROM work_sessions \
                 ORDER BY companion_id, updated_at DESC, id"
            ),
            [],
            WorkSession::from_row,
        )
    }

    pub fn work_session(&self, id: &str) -> Result<Option<WorkSession>> {
        self.0.fetch(
            &format!("SELECT {SESSION_COLUMNS} FROM work_sessions WHERE id = ?1"),
            [id],
            WorkSession::from_row,
        )
    }

    pub fn insert_work_session(&self, session: &WorkSession) -> Result<()> {
        let stamp = now();
        self.0
            .conn()
            .prepare_cached(&format!(
                "INSERT INTO work_sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ))?
            .execute(params![
                session.id,
                session.companion_id.as_str(),
                session.title,
                session.status.as_str(),
                session.client_id,
                or_stamp(&session.created_at, &stamp),
                or_stamp(&session.updated_at, &stamp),
            ])?;
        Ok(())
    }

    pub fn set_work_status(&self, id: &str, status: WorkStatus) -> Result<()> {
        let changed = self
            .0
            .conn()
            .prepare_cached("UPDATE work_sessions SET status = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![id, status.as_str(), now()])?;
        require_found(changed, "work session not found")
    }

    pub fn close_work(&self, id: &str) -> Result<()> {
        let session = self
            .work_session(id)?
            .ok_or_else(|| StoreError::NotFound("work session not found".into()))?;
        if session.status == WorkStatus::Closed {
            return Ok(());
        }
        self.set_work_status(id, WorkStatus::Closed)
    }

    /// Reads the run history, including any currently pending approval.
    pub fn runs(&self, work: &str) -> Result<Vec<Run>> {
        self.0.fetch_all(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_run_view WHERE work_id = ?1 ORDER BY started_at, run_id"
            ),
            [work],
            Run::from_row,
        )
    }

    pub fn run(&self, run_id: &str) -> Result<Option<Run>> {
        self.0.fetch(
            &format!("SELECT {RUN_COLUMNS} FROM work_run_view WHERE run_id = ?1"),
            [run_id],
            Run::from_row,
        )
    }

    /// The run a session is currently executing, if any. At most one run per
    /// session is open.
    pub fn active_run(&self, work: &str) -> Result<Option<Run>> {
        self.0.fetch(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_run_view WHERE run_id = (
                   SELECT run_id FROM work_runs WHERE work_id = ?1 AND state IN ('starting','running')
                   ORDER BY started_at DESC LIMIT 1
                 )"
            ),
            [work],
            Run::from_row,
        )
    }

    pub fn run_executor(&self, run_id: &str) -> Result<Option<RunExecutor>> {
        self.0.fetch(
            "SELECT executor FROM work_runs WHERE run_id = ?1",
            [run_id],
            |row| json_column(row, "executor", "invalid stored work executor"),
        )
    }

    pub fn input_receipt(&self, work_id: &str, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        self.0.fetch(
            "SELECT payload_hash, request_id FROM work_input_receipts WHERE work_id = ?1 AND request_key = ?2",
            [work_id, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    pub fn record_input(&self, work_id: &str, receipt: WorkInputReceipt<'_>) -> Result<()> {
        insert_input(&self.0.conn(), work_id, receipt)
    }

    pub fn insert_run(
        &self,
        run: &Run,
        executor: &RunExecutor,
        receipt: Option<WorkInputReceipt<'_>>,
    ) -> Result<()> {
        let stamp = now();
        self.0.write_tx(|transaction| {
            if open_state(run.state) {
                let changed = transaction.prepare_cached(
                    "UPDATE work_sessions SET status = 'active', updated_at = ?2 WHERE id = ?1 AND status <> 'closed'",
                )?.execute(params![run.work_id, stamp])?;
                require_found(changed, "no open work session")?;
            }
            transaction.prepare_cached(
                "INSERT INTO work_runs (run_id, work_id, instruction, state, failure, started_at, updated_at, executor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            )?
            .execute(params![
                run.run_id,
                run.work_id,
                run.instruction,
                stored_run_state(run.state),
                run.failure.as_ref().map(to_json).transpose()?,
                or_stamp(&run.started_at, &stamp),
                stamp,
                to_json(executor)?,
            ])?;
            if let Some(receipt) = receipt {
                insert_input(transaction, &run.work_id, receipt)?;
            }
            Ok(())
        })
    }

    pub fn update_run(&self, run: &Run) -> Result<()> {
        self.0.write_tx(|transaction| {
            let now = now();
            let changed = transaction
                .prepare_cached(
                    "UPDATE work_runs SET state = ?2, failure = ?3, updated_at = ?4 WHERE run_id = ?1",
                )?
                .execute(params![
                    run.run_id,
                    stored_run_state(run.state),
                    run.failure.as_ref().map(to_json).transpose()?,
                    now,
                ])?;
            require_found(changed, "run not found")?;
            if run.state.is_terminal() {
                let failure = Failure::new(
                    "work_run_finished",
                    "the operation was still queued when its work run ended",
                );
                transaction.prepare_cached(
                    "UPDATE device_operations SET status = 'failed', failure = ?2, updated_at = ?3
                     WHERE run_id = ?1 AND status = 'pending'",
                )?.execute(params![run.run_id, to_json(&failure)?, now])?;
            }
            Ok(())
        })
    }
}

fn insert_input(
    connection: &rusqlite::Connection,
    work_id: &str,
    receipt: WorkInputReceipt<'_>,
) -> Result<()> {
    connection.prepare_cached(
        "INSERT INTO work_input_receipts (work_id, request_key, payload_hash, request_id) VALUES (?1, ?2, ?3, ?4)",
    )?.execute(params![work_id, receipt.request_key, receipt.payload_hash, receipt.request_id])?;
    Ok(())
}
