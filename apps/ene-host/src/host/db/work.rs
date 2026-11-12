//! Work-session rows. Runs live in the Rig conversation log.
use super::Store;
use crate::host::{
    db::Result,
    error::HostError,
    work::{WorkSession, WorkStatus},
};
use rusqlite::{Connection, OptionalExtension, params};

impl Store {
    pub(crate) fn work(&self, id: &str) -> Result<Option<WorkSession>> {
        let id = id.to_owned();
        self.call(move |connection| read_work(connection, &id).map_err(Into::into))
    }

    pub(crate) fn works(&self) -> Result<Vec<WorkSession>> {
        self.call(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM work_sessions ORDER BY created_at, id"
            ))?;
            statement
                .query_map([], row_to_work)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn works_for(&self, companion: &str) -> Result<Vec<WorkSession>> {
        let companion = companion.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM work_sessions WHERE companion_id=?1 ORDER BY created_at, id"
            ))?;
            statement
                .query_map([&companion], row_to_work)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn save_work(&self, work: &WorkSession) -> Result<()> {
        let work = work.clone();
        self.call(move |connection| {
            write_work(connection, &work)?;
            Ok(())
        })
    }
}

const COLUMNS: &str =
    "id, companion_id, title, status, reported_run, closed, created_at, updated_at";

fn read_work(connection: &Connection, id: &str) -> rusqlite::Result<Option<WorkSession>> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM work_sessions WHERE id=?1"),
            [id],
            row_to_work,
        )
        .optional()
}

fn write_work(connection: &Connection, work: &WorkSession) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO work_sessions(id, companion_id, title, status, reported_run, closed, \
         created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
         ON CONFLICT(id) DO UPDATE SET companion_id=excluded.companion_id, title=excluded.title, \
         status=excluded.status, reported_run=excluded.reported_run, closed=excluded.closed, \
         updated_at=excluded.updated_at",
        params![
            work.id,
            work.companion_id,
            work.title,
            work.status.as_str(),
            work.reported_run,
            i64::from(work.closed),
            work.created_at,
            work.updated_at,
        ],
    )?;
    Ok(())
}

fn row_to_work(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkSession> {
    let status: String = row.get(3)?;
    Ok(WorkSession {
        id: row.get(0)?,
        companion_id: row.get(1)?,
        title: row.get(2)?,
        status: WorkStatus::parse(&status),
        reported_run: row.get(4)?,
        closed: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[allow(dead_code)]
fn _uses(_: &HostError) {}

impl Store {
    pub(crate) fn pending_inquiries(&self, work_id: &str) -> Result<Vec<ene_rpc::WorkInquiry>> {
        let work_id = work_id.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT request_id, prompt, choices FROM work_inquiries \
                 WHERE work_id=?1 AND answered_at IS NULL ORDER BY created_at, id",
            )?;
            statement
                .query_map([&work_id], |row| {
                    let choices: String = row.get(2)?;
                    Ok(ene_rpc::WorkInquiry {
                        request_id: row.get(0)?,
                        prompt: row.get(1)?,
                        choices: serde_json::from_str(&choices).unwrap_or_default(),
                        ..Default::default()
                    })
                })?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn add_inquiry(
        &self,
        work_id: &str,
        request_id: &str,
        prompt: &str,
        choices: &[String],
    ) -> Result<()> {
        let work_id = work_id.to_owned();
        let request_id = request_id.to_owned();
        let prompt = prompt.to_owned();
        let choices = serde_json::to_string(choices)
            .map_err(|error| crate::host::error::HostError::internal(error.to_string()))?;
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO work_inquiries(id, work_id, request_id, prompt, choices, \
                 created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    crate::host::db::new_id(),
                    work_id,
                    request_id,
                    prompt,
                    choices,
                    crate::host::db::now(),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn answer_inquiry(&self, work_id: &str, request_id: &str) -> Result<bool> {
        let work_id = work_id.to_owned();
        let request_id = request_id.to_owned();
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE work_inquiries SET answered_at=?3 \
                 WHERE work_id=?1 AND request_id=?2 AND answered_at IS NULL",
                rusqlite::params![work_id, request_id, crate::host::db::now()],
            )?;
            Ok(changed > 0)
        })
    }
}
