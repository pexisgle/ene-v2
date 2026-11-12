//! Schedule rows.
use super::Store;
use crate::host::{db::Result, schedule::Schedule};
use rusqlite::{OptionalExtension, params};

impl Store {
    pub(crate) fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        let id = id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {COLUMNS} FROM schedules WHERE id=?1"),
                    [&id],
                    row_to_schedule,
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub(crate) fn schedules_for(&self, companion: &str) -> Result<Vec<Schedule>> {
        let companion = companion.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM schedules WHERE companion_id=?1 ORDER BY created_at, id"
            ))?;
            statement
                .query_map([&companion], row_to_schedule)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn save_schedule(&self, schedule: &Schedule) -> Result<()> {
        let schedule = schedule.clone();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO schedules(id, companion_id, cron_expr, run_at, title, instruction, \
                 enabled, created_at, last_triggered_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
                 ON CONFLICT(id) DO UPDATE SET companion_id=excluded.companion_id, \
                 cron_expr=excluded.cron_expr, run_at=excluded.run_at, title=excluded.title, \
                 instruction=excluded.instruction, enabled=excluded.enabled, \
                 last_triggered_at=excluded.last_triggered_at",
                params![
                    schedule.id,
                    schedule.companion_id,
                    schedule.cron_expr,
                    schedule.run_at,
                    schedule.title,
                    schedule.instruction,
                    i64::from(schedule.enabled),
                    schedule.created_at,
                    schedule.last_triggered_at,
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn remove_schedule(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call(move |connection| {
            connection.execute("DELETE FROM schedules WHERE id=?1", [&id])?;
            Ok(())
        })
    }
}

const COLUMNS: &str = "id, companion_id, cron_expr, run_at, title, instruction, enabled, created_at, last_triggered_at";

fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        companion_id: row.get(1)?,
        cron_expr: row.get(2)?,
        run_at: row.get(3)?,
        title: row.get(4)?,
        instruction: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        last_triggered_at: row.get(8)?,
    })
}
