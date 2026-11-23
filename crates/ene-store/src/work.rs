//! Work sessions and runs persistence.

use crate::{
    Store, StoreError, enum_name_column,
    error::Result,
    optional_json_column, require_found, to_json,
    util::{now, or_stamp},
};
use ene_domain::{Failure, Run, RunState, WorkSession, WorkStatus};
use rusqlite::{Row, params};

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<WorkSession> {
    Ok(WorkSession {
        id: row.get("id")?,
        companion_id: row.get("companion_id")?,
        title: row.get("title")?,
        status: enum_name_column(row, "status", "unknown work status")?,
        client_id: row.get("client_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SESSION_COLUMNS: &str = "id, companion_id, title, status, client_id, created_at, updated_at";

fn row_to_run(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        run_id: row.get("run_id")?,
        work_id: row.get("work_id")?,
        instruction: row.get("instruction")?,
        state: enum_name_column(row, "state", "unknown run state")?,
        pending_request_id: row.get("pending_request_id")?,
        failure: optional_json_column::<Failure>(row, "failure", "stored failure is corrupt")?,
        started_at: row.get("started_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const RUN_COLUMNS: &str =
    "run_id, work_id, instruction, state, pending_request_id, failure, started_at, updated_at";

/// A run is open while it can still produce events.
pub fn open_state(state: RunState) -> bool {
    matches!(
        state,
        RunState::Starting | RunState::Running | RunState::WaitingInput
    )
}

/// Repository managing `work_sessions` and `work_runs`.
#[derive(Clone, Copy, Debug)]
pub struct WorkRepo<'a>(&'a Store);

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
            row_to_session,
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
            row_to_session,
        )
    }

    pub fn work_session(&self, id: &str) -> Result<Option<WorkSession>> {
        self.0.fetch(
            &format!("SELECT {SESSION_COLUMNS} FROM work_sessions WHERE id = ?1"),
            [id],
            row_to_session,
        )
    }

    pub fn insert_work_session(&self, session: &WorkSession) -> Result<()> {
        let stamp = now();
        self.0
            .conn()?
            .prepare_cached(&format!(
                "INSERT INTO work_sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ))?
            .execute(params![
                session.id,
                session.companion_id,
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
            .conn()?
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

    pub fn runs(&self, work: &str) -> Result<Vec<Run>> {
        self.0.fetch_all(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_runs WHERE work_id = ?1 ORDER BY started_at, run_id"
            ),
            [work],
            row_to_run,
        )
    }

    pub fn run(&self, run_id: &str) -> Result<Option<Run>> {
        self.0.fetch(
            &format!("SELECT {RUN_COLUMNS} FROM work_runs WHERE run_id = ?1"),
            [run_id],
            row_to_run,
        )
    }

    /// The run a session is currently executing, if any. At most one run per
    /// session is open.
    pub fn active_run(&self, work: &str) -> Result<Option<Run>> {
        self.0.fetch(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_runs WHERE work_id = ?1 \
                 AND state IN ('starting','running','waiting_input') \
                 ORDER BY started_at DESC LIMIT 1"
            ),
            [work],
            row_to_run,
        )
    }

    pub fn insert_run(&self, run: &Run) -> Result<()> {
        let stamp = now();
        self.0
            .conn()?
            .prepare_cached(&format!(
                "INSERT INTO work_runs ({RUN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ))?
            .execute(params![
                run.run_id,
                run.work_id,
                run.instruction,
                run.state.as_str(),
                run.pending_request_id,
                run.failure.as_ref().map(to_json).transpose()?,
                or_stamp(&run.started_at, &stamp),
                stamp,
            ])?;
        Ok(())
    }

    pub fn update_run(&self, run: &Run) -> Result<()> {
        let changed = self
            .0
            .conn()?
            .prepare_cached(
                "UPDATE work_runs SET state = ?2, pending_request_id = ?3, failure = ?4, \
                 updated_at = ?5 WHERE run_id = ?1",
            )?
            .execute(params![
                run.run_id,
                run.state.as_str(),
                run.pending_request_id,
                run.failure.as_ref().map(to_json).transpose()?,
                now(),
            ])?;
        require_found(changed, "run not found")
    }
}
