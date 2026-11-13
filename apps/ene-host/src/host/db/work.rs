//! Work sessions, runs and inquiries.
use super::{
    Result, Store, enum_column, json_column, now, optional_json_column, or_stamp, require_found,
    to_json,
};
use crate::host::error::{Failure, HostError};
use crate::host::work::{Inquiry, Run, RunState, WorkSession, WorkStatus};
use rusqlite::{Row, params};

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<WorkSession> {
    Ok(WorkSession {
        id: row.get("id")?,
        companion_id: row.get("companion_id")?,
        title: row.get("title")?,
        status: enum_column(row, "status", "unknown work status", WorkStatus::parse)?,
        client_id: row.get("client_id")?,
        activation_revision: row.get::<_, u64>("activation_revision")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SESSION_COLUMNS: &str =
    "id, companion_id, title, status, client_id, activation_revision, created_at, updated_at";

fn row_to_run(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        run_id: row.get("run_id")?,
        work_id: row.get("work_id")?,
        instruction: row.get("instruction")?,
        state: enum_column(row, "state", "unknown run state", RunState::parse)?,
        pending_request_id: row.get("pending_request_id")?,
        failure: optional_json_column::<Failure>(row, "failure", "stored failure is corrupt")?,
        cancellation_requested_at: row.get("cancellation_requested_at")?,
        cancellation_reason: row.get("cancellation_reason")?,
        unverified_pids: json_column(row, "unverified_pids", "stored pid list is corrupt")?,
        started_at: row.get("started_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const RUN_COLUMNS: &str = "run_id, work_id, instruction, state, pending_request_id, failure, \
     cancellation_requested_at, cancellation_reason, unverified_pids, started_at, updated_at";

const INQUIRY_COLUMNS: &str =
    "request_id, work_id, run_id, prompt, choices, answer, created_at, resolved_at";

fn row_to_inquiry(row: &Row<'_>) -> rusqlite::Result<Inquiry> {
    Ok(Inquiry {
        request_id: row.get("request_id")?,
        work_id: row.get("work_id")?,
        run_id: row.get("run_id")?,
        prompt: row.get("prompt")?,
        choices: json_column(row, "choices", "stored inquiry choices are corrupt")?,
        answer: row.get("answer")?,
        created_at: row.get("created_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

/// A run is open while it can still produce events.
pub(crate) fn open_state(state: RunState) -> bool {
    matches!(
        state,
        RunState::Starting | RunState::Running | RunState::WaitingInput | RunState::Cancelling
    )
}

impl Store {
    pub(crate) fn work_sessions(&self, companion: &str) -> Result<Vec<WorkSession>> {
        self.fetch_all(
            &format!(
                "SELECT {SESSION_COLUMNS} FROM work_sessions WHERE companion_id = ?1 \
                 ORDER BY updated_at DESC, id"
            ),
            [companion],
            row_to_session,
        )
    }

    pub(crate) fn work_session(&self, id: &str) -> Result<Option<WorkSession>> {
        self.fetch(
            &format!("SELECT {SESSION_COLUMNS} FROM work_sessions WHERE id = ?1"),
            [id],
            row_to_session,
        )
    }

    pub(crate) fn insert_work_session(&self, session: &WorkSession) -> Result<()> {
        let stamp = now();
        self.conn()?.execute(
            &format!(
                "INSERT INTO work_sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                session.id,
                session.companion_id,
                session.title,
                session.status.as_str(),
                session.client_id,
                session.activation_revision,
                or_stamp(&session.created_at, &stamp),
                or_stamp(&session.updated_at, &stamp),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn set_work_status(&self, id: &str, status: WorkStatus) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE work_sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), now()],
        )?;
        require_found(changed, "work session not found")
    }

    pub(crate) fn close_work(&self, id: &str) -> Result<()> {
        let session = self
            .work_session(id)?
            .ok_or_else(|| HostError::not_found("work session not found"))?;
        if session.status == WorkStatus::Closed {
            return Ok(());
        }
        self.set_work_status(id, WorkStatus::Closed)
    }

    pub(crate) fn runs(&self, work: &str) -> Result<Vec<Run>> {
        self.fetch_all(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_runs WHERE work_id = ?1 ORDER BY started_at, run_id"
            ),
            [work],
            row_to_run,
        )
    }

    pub(crate) fn run(&self, run_id: &str) -> Result<Option<Run>> {
        self.fetch(
            &format!("SELECT {RUN_COLUMNS} FROM work_runs WHERE run_id = ?1"),
            [run_id],
            row_to_run,
        )
    }

    /// The run a session is currently executing, if any. At most one run per
    /// session is open.
    pub(crate) fn active_run(&self, work: &str) -> Result<Option<Run>> {
        self.fetch(
            &format!(
                "SELECT {RUN_COLUMNS} FROM work_runs WHERE work_id = ?1 \
                 AND state IN ('starting','running','waiting_input','cancelling') \
                 ORDER BY started_at DESC LIMIT 1"
            ),
            [work],
            row_to_run,
        )
    }

    pub(crate) fn insert_run(&self, run: &Run) -> Result<()> {
        let stamp = now();
        self.conn()?.execute(
            &format!(
                "INSERT INTO work_runs ({RUN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                run.run_id,
                run.work_id,
                run.instruction,
                run.state.as_str(),
                run.pending_request_id,
                run.failure.as_ref().map(to_json).transpose()?,
                run.cancellation_requested_at,
                run.cancellation_reason,
                to_json(&run.unverified_pids)?,
                or_stamp(&run.started_at, &stamp),
                stamp,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn update_run(&self, run: &Run) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE work_runs SET state = ?2, pending_request_id = ?3, failure = ?4, \
             cancellation_requested_at = ?5, cancellation_reason = ?6, unverified_pids = ?7, \
             updated_at = ?8 WHERE run_id = ?1",
            params![
                run.run_id,
                run.state.as_str(),
                run.pending_request_id,
                run.failure.as_ref().map(to_json).transpose()?,
                run.cancellation_requested_at,
                run.cancellation_reason,
                to_json(&run.unverified_pids)?,
                now(),
            ],
        )?;
        require_found(changed, "run not found")
    }

    pub(crate) fn inquiries(&self, work: &str) -> Result<Vec<Inquiry>> {
        self.fetch_all(
            &format!(
                "SELECT {INQUIRY_COLUMNS} FROM work_inquiries WHERE work_id = ?1 ORDER BY created_at"
            ),
            [work],
            row_to_inquiry,
        )
    }

    pub(crate) fn pending_inquiries(&self, work: &str) -> Result<Vec<Inquiry>> {
        Ok(self
            .inquiries(work)?
            .into_iter()
            .filter(Inquiry::is_pending)
            .collect())
    }

    pub(crate) fn inquiry(&self, request_id: &str) -> Result<Option<Inquiry>> {
        self.fetch(
            &format!("SELECT {INQUIRY_COLUMNS} FROM work_inquiries WHERE request_id = ?1"),
            [request_id],
            row_to_inquiry,
        )
    }

    pub(crate) fn insert_inquiry(&self, inquiry: &Inquiry) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO work_inquiries (request_id, work_id, run_id, prompt, choices, answer, \
             created_at, resolved_at) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
            params![
                inquiry.request_id,
                inquiry.work_id,
                inquiry.run_id,
                inquiry.prompt,
                to_json(&inquiry.choices)?,
                or_stamp(&inquiry.created_at, &now()),
            ],
        )?;
        Ok(())
    }

    /// Answers one pending inquiry. A second answer is a conflict, so the same
    /// question cannot be answered twice.
    pub(crate) fn resolve_inquiry(&self, request_id: &str, answer: &str) -> Result<Inquiry> {
        if answer.trim().is_empty() || answer.chars().count() > 16_000 {
            return Err(HostError::invalid("answer must be 1-16000 characters"));
        }
        self.fetch(
            &format!(
                "UPDATE work_inquiries SET answer = ?2, resolved_at = ?3 \
                 WHERE request_id = ?1 AND resolved_at IS NULL RETURNING {INQUIRY_COLUMNS}"
            ),
            params![request_id, answer, now()],
            row_to_inquiry,
        )?
        .ok_or_else(|| HostError::conflict("inquiry is missing or already answered"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> WorkSession {
        WorkSession {
            id: id.into(),
            companion_id: "ene".into(),
            title: "task".into(),
            status: WorkStatus::Idle,
            client_id: Some("c1".into()),
            activation_revision: 3,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn run(id: &str, work: &str, state: RunState) -> Run {
        Run {
            run_id: id.into(),
            work_id: work.into(),
            instruction: "do it".into(),
            state,
            pending_request_id: None,
            failure: None,
            cancellation_requested_at: None,
            cancellation_reason: None,
            unverified_pids: Vec::new(),
            started_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn sessions_runs_and_inquiries_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        store.insert_work_session(&session("w1")).unwrap();
        let loaded = store.work_session("w1").unwrap().unwrap();
        assert_eq!(loaded.client_id.as_deref(), Some("c1"));
        assert_eq!(loaded.activation_revision, 3);
        store
            .insert_run(&run("r1", "w1", RunState::Running))
            .unwrap();
        assert_eq!(store.active_run("w1").unwrap().unwrap().run_id, "r1");
        let mut current = store.run("r1").unwrap().unwrap();
        current.state = RunState::WaitingInput;
        current.pending_request_id = Some("q1".into());
        store.update_run(&current).unwrap();
        assert_eq!(
            store
                .active_run("w1")
                .unwrap()
                .unwrap()
                .pending_request_id
                .as_deref(),
            Some("q1")
        );
        store
            .insert_inquiry(&Inquiry {
                request_id: "q1".into(),
                work_id: "w1".into(),
                run_id: "r1".into(),
                prompt: "which?".into(),
                choices: vec!["a".into(), "b".into()],
                answer: None,
                created_at: String::new(),
                resolved_at: None,
            })
            .unwrap();
        assert_eq!(store.pending_inquiries("w1").unwrap().len(), 1);
        store.resolve_inquiry("q1", "a").unwrap();
        assert!(store.pending_inquiries("w1").unwrap().is_empty());
        assert!(store.resolve_inquiry("q1", "b").is_err());
        store.close_work("w1").unwrap();
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Closed
        );
        assert!(open_state(RunState::Cancelling));
        assert!(!open_state(RunState::Completed));
    }
}
