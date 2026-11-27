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
const CHANGED_RUN_COLUMNS: &str = "run_id, work_id, instruction, state, NULL AS pending_request_id, failure, started_at, updated_at";

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

/// The original receipt and its durable execution record, if available.
#[derive(Debug)]
pub struct AcceptedWorkInput {
    pub payload_hash: Vec<u8>,
    pub request_id: String,
    pub run_id: Option<String>,
}

impl<'a> WorkRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn work_sessions(&self, companion: &str) -> Result<Vec<WorkSession>> {
        self.0.fetch_all(
            &format!(
                "SELECT {SESSION_COLUMNS} FROM work_session_view WHERE companion_id = ?1 \
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
                "SELECT {SESSION_COLUMNS} FROM work_session_view \
                 ORDER BY companion_id, updated_at DESC, id"
            ),
            [],
            WorkSession::from_row,
        )
    }

    pub fn work_session(&self, id: &str) -> Result<Option<WorkSession>> {
        self.0.fetch(
            &format!("SELECT {SESSION_COLUMNS} FROM work_session_view WHERE id = ?1"),
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
                if session.status == WorkStatus::Closed {
                    "closed"
                } else {
                    "idle"
                },
                session.client_id,
                or_stamp(&session.created_at, &stamp),
                or_stamp(&session.updated_at, &stamp),
            ])?;
        Ok(())
    }

    pub fn close_work(&self, id: &str) -> Result<()> {
        let changed = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE work_sessions SET status = 'closed',
                    updated_at = CASE WHEN status = 'closed' THEN updated_at ELSE ?2 END WHERE id = ?1",
            )?
            .execute(params![id, now()])?;
        require_found(changed, "work session not found")
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
                   SELECT run_id FROM work_runs WHERE work_id = ?1 AND state = 'running'
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

    /// Admits the oldest accepted input once, only while the session can run it.
    pub fn promote_starting(&self, work_id: &str) -> Result<Option<Run>> {
        self.0.fetch(
            &format!(
                "UPDATE work_runs SET state = 'running', updated_at = ?2 WHERE rowid = (
                   SELECT rowid FROM work_runs WHERE work_id = ?1 AND state = 'starting'
                   ORDER BY rowid LIMIT 1
                 )
                 AND EXISTS (SELECT 1 FROM work_sessions WHERE id = ?1 AND status <> 'closed')
                 AND NOT EXISTS (SELECT 1 FROM work_runs WHERE work_id = ?1 AND state = 'running')
                 RETURNING {CHANGED_RUN_COLUMNS}"
            ),
            params![work_id, now()],
            Run::from_row,
        )
    }

    /// Settles accepted inputs that have never been dispatched.
    pub fn cancel_starting(&self, work_id: &str) -> Result<Vec<Run>> {
        self.0.fetch_all(
            &format!(
                "UPDATE work_runs SET state = 'cancelled', updated_at = ?2
                 WHERE work_id = ?1 AND state = 'starting' RETURNING {CHANGED_RUN_COLUMNS}"
            ),
            params![work_id, now()],
            Run::from_row,
        )
    }

    /// Records the interruption of all undispatched runs on startup.
    pub fn interrupt_starting(&self, failure: &Failure) -> Result<()> {
        self.0.conn().execute(
            "UPDATE work_runs SET state = 'failed', failure = ?1, updated_at = ?2
             WHERE state = 'starting'",
            params![to_json(failure)?, now()],
        )?;
        Ok(())
    }

    pub fn input_receipt(&self, work_id: &str, key: &str) -> Result<Option<AcceptedWorkInput>> {
        self.0.fetch(
            "SELECT payload_hash, request_id, run_id FROM work_input_receipts WHERE work_id = ?1 AND request_key = ?2",
            [work_id, key],
            |row| Ok(AcceptedWorkInput {
                payload_hash: row.get(0)?,
                request_id: row.get(1)?,
                run_id: row.get(2)?,
            }),
        )
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
                    "UPDATE work_sessions SET updated_at = ?2 WHERE id = ?1 AND status <> 'closed'",
                )?.execute(params![run.work_id, stamp])?;
                require_found(changed, "no open work session")?;
            }
            let inserted = transaction.prepare_cached(
                "INSERT INTO work_runs (run_id, work_id, instruction, state, failure, started_at, updated_at, executor)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE ?4 <> 'running'
                   OR NOT EXISTS (SELECT 1 FROM work_runs WHERE work_id = ?2 AND state = 'running')"
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
            if inserted == 0 {
                return Err(StoreError::Conflict("the work session already has a running turn".into()));
            }
            if let Some(receipt) = receipt {
                transaction.prepare_cached(
                    "INSERT INTO work_input_receipts
                     (work_id, request_key, payload_hash, request_id, run_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?.execute(params![
                    run.work_id, receipt.request_key, receipt.payload_hash, receipt.request_id, run.run_id
                ])?;
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
                transaction.prepare_cached(
                    "UPDATE work_sessions SET updated_at = ?2 WHERE id = ?1 AND status <> 'closed'",
                )?.execute(params![run.work_id, now])?;
                let failure = Failure::new(
                    "work_run_finished",
                    "the operation was still queued when its work run ended",
                );
                transaction.prepare_cached(
                    "UPDATE device_operations
                     SET status = CASE status WHEN 'pending' THEN 'failed' ELSE 'uncertain' END,
                         failure = CASE status WHEN 'pending' THEN ?2 END, updated_at = ?3
                     WHERE run_id = ?1 AND status IN ('pending','dispatched')",
                )?.execute(params![run.run_id, to_json(&failure)?, now])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[test]
    fn work_wait_state_is_recovered_from_current_approvals() {
        use ene_domain::{RunState, WorkStatus};
        use ene_policy::ApprovalStatus;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
        store.conn().execute_batch(
            "INSERT INTO work_sessions (id, companion_id, title, status, created_at, updated_at)
                  VALUES ('work', 'ene', 'with run', 'idle', '2026-01-01', '2026-01-01'),
                         ('closed', 'ene', 'closed', '2026-01-01', '2026-01-01');
             INSERT INTO work_runs (run_id, work_id, instruction, state, started_at, updated_at)
                  VALUES ('run', 'work', 'continue', 'running', '2026-01-01', '2026-01-01');
             INSERT INTO approval_requests (request_id, session_id, run_id, work_id,
                          action_payload_hash, tool_name, normalized_arguments, status, created_at)
                  VALUES ('later', 'ene', 'run', 'work', X'01', 'device_request', '{}', 'pending', '2026-01-02'),
                         ('first', 'ene', 'run', 'work', X'01', 'device_request', '{}', 'pending', '2026-01-01'),
                         ('auto', 'ene', 'run', 'work', X'01', 'device_request', '{}', 'evaluating', '2026-01-01'),
                         ('different', 'ene', 'run', 'other', X'01', 'device_request', '{}', 'pending', '2026-01-01');",
        ).unwrap();
        let work = store.work_repo();
        let state = |id| work.work_session(id).unwrap().unwrap().status;
        assert_eq!(state("closed"), WorkStatus::Closed);
        assert_eq!(state("work"), WorkStatus::Active);
        let run = work.active_run("work").unwrap().unwrap();
        assert_eq!(run.state, RunState::WaitingInput);
        assert_eq!(run.pending_request_id.as_deref(), Some("first"));
        assert_eq!(work.runs("work").unwrap(), [run]);

        let approvals = store.approval_repo();
        approvals
            .resolve_approval("first", ApprovalStatus::Denied)
            .unwrap();
        let mut run = work.run("run").unwrap().unwrap();
        assert_eq!(run.pending_request_id.as_deref(), Some("later"));
        approvals
            .resolve_approval("later", ApprovalStatus::Approved)
            .unwrap();
        run = work.run("run").unwrap().unwrap();
        assert_eq!(run.state, RunState::Running);
        assert!(run.pending_request_id.is_none());

        approvals.request_user_approval("auto").unwrap();
        run.state = RunState::Cancelled;
        work.update_run(&run).unwrap();
        assert_eq!(state("work"), WorkStatus::Idle);
        let terminal = work.run("run").unwrap().unwrap();
        assert_eq!(terminal.state, RunState::Cancelled);
        assert!(terminal.pending_request_id.is_none());
    }
}
