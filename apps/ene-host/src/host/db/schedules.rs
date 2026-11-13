//! Schedules: stored reminders for the head. Firing them is not wired yet.
use super::{Result, Store, now};
use crate::host::error::HostError;
use rusqlite::{OptionalExtension, params};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) instruction: String,
    pub(crate) run_at: Option<String>,
    pub(crate) cron_expr: Option<String>,
    pub(crate) enabled: bool,
}

impl Store {
    pub(crate) fn insert_schedule(
        &self,
        companion: &str,
        title: &str,
        instruction: &str,
        run_at: Option<&str>,
        cron_expr: Option<&str>,
    ) -> Result<Schedule> {
        if run_at.is_none() && cron_expr.is_none() {
            return Err(HostError::invalid("a schedule needs run_at or cron_expr"));
        }
        if title.chars().count() > 200 || instruction.chars().count() > 4_000 {
            return Err(HostError::invalid("schedule text is too long"));
        }
        if let Some(run_at) = run_at
            && super::epoch(run_at).is_none()
        {
            return Err(HostError::invalid("run_at must be an RFC 3339 time"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn()?.execute(
            "INSERT INTO schedules (id, companion_id, cron_expr, run_at, title, instruction, \
             enabled, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
            params![id, companion, cron_expr, run_at, title, instruction, now()],
        )?;
        self.schedule(&id)?
            .ok_or_else(|| HostError::storage("schedule insert did not persist"))
    }

    fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        let connection = self.conn()?;
        let schedule = connection
            .query_row(
                "SELECT id, title, instruction, run_at, cron_expr, enabled \
                 FROM schedules WHERE id = ?1",
                [id],
                row_to_schedule,
            )
            .optional()?;
        Ok(schedule)
    }

    pub(crate) fn schedules(&self, companion: &str) -> Result<Vec<Schedule>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT id, title, instruction, run_at, cron_expr, enabled FROM schedules \
             WHERE companion_id = ?1 AND enabled = 1 ORDER BY created_at",
        )?;
        let rows = statement.query_map([companion], row_to_schedule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn cancel_schedule(&self, companion: &str, id: &str) -> Result<bool> {
        let changed = self.conn()?.execute(
            "DELETE FROM schedules WHERE id = ?1 AND companion_id = ?2",
            params![id, companion],
        )?;
        Ok(changed > 0)
    }
}

fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get("id")?,
        title: row.get("title")?,
        instruction: row.get("instruction")?,
        run_at: row.get("run_at")?,
        cron_expr: row.get("cron_expr")?,
        enabled: row.get("enabled")?,
    })
}
