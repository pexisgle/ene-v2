//! Schedules: stored reminders and idempotent materialized occurrences.

#[cfg(test)]
mod tests;

use crate::row::{FromRow, row_fields};
use crate::{Store, StoreError, error::Result};
use ene_domain::schedule::{ScheduleTiming, next_occurrence, parse_run_at};
use jiff::{Timestamp, tz::TimeZone};
use rusqlite::{Connection, params};

/// The wall clock schedules are read against: the local time zone and the
/// current instant (optionally frozen in tests for determinism).
#[derive(Clone, Debug)]
pub struct ScheduleClock {
    zone: TimeZone,
    frozen: Option<Timestamp>,
    fallback: bool,
}

impl Default for ScheduleClock {
    fn default() -> Self {
        Self::system()
    }
}

impl ScheduleClock {
    /// The host's system time zone, or UTC with a visible warning when the
    /// platform does not name one.
    pub fn system() -> Self {
        match TimeZone::try_system() {
            Ok(zone) => Self {
                zone,
                frozen: None,
                fallback: false,
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the host's time zone is unavailable; schedules will be read in UTC"
                );
                Self {
                    zone: TimeZone::UTC,
                    frozen: None,
                    fallback: true,
                }
            }
        }
    }

    /// A clock frozen at `now` in `zone`, for deterministic tests.
    pub fn fixed(zone: TimeZone, now: Timestamp) -> Self {
        Self {
            zone,
            frozen: Some(now),
            fallback: false,
        }
    }

    pub fn zone(&self) -> &TimeZone {
        &self.zone
    }

    pub fn now(&self) -> Timestamp {
        self.frozen.unwrap_or_else(Timestamp::now)
    }

    /// How the zone is named to callers in tool results.
    pub fn name(&self) -> &str {
        if self.fallback {
            "UTC (system time zone unavailable)"
        } else {
            self.zone.iana_name().unwrap_or("local")
        }
    }
}

fn stamp(at: Timestamp) -> String {
    format!("{at:.0}")
}

/// Stored reminder schedule row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub id: String,
    pub companion_id: String,
    pub title: String,
    pub instruction: String,
    pub run_at: Option<String>,
    pub cron_expr: Option<String>,
    pub enabled: bool,
    /// The next firing, UTC RFC 3339; `None` once a one-shot has fired.
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub last_triggered_at: Option<String>,
}

row_fields!(
    Schedule,
    SCHEDULE_FIELDS {
        id,
        companion_id,
        title,
        instruction,
        run_at,
        cron_expr,
        enabled,
        next_run_at,
        created_at,
        last_triggered_at,
    }
);

impl Schedule {
    /// Whether the row fires once and is then disabled.
    pub fn one_shot(&self) -> bool {
        self.run_at.is_some()
    }
}

/// Parameters for registering a new schedule.
#[derive(Clone, Copy, Debug)]
pub struct NewSchedule<'a> {
    pub companion: &'a str,
    pub title: &'a str,
    pub instruction: &'a str,
    pub run_at: Option<&'a str>,
    pub cron_expr: Option<&'a str>,
}

/// Persisted delivery record for one materialized schedule firing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleOccurrence {
    pub occurrence_key: String,
    pub schedule_id: String,
    pub companion_id: String,
    pub scheduled_for: String,
    pub title: String,
    pub instruction: String,
    pub status: String,
    pub attempts: u64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

row_fields!(
    ScheduleOccurrence,
    OCCURRENCE_FIELDS {
        occurrence_key,
        schedule_id,
        companion_id,
        scheduled_for,
        title,
        instruction,
        status,
        attempts,
        last_error,
        created_at,
        delivered_at,
    }
);

fn pending_occurrences(connection: &Connection) -> Result<Vec<ScheduleOccurrence>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {OCCURRENCE_FIELDS} FROM schedule_occurrences WHERE status = 'pending' \
         ORDER BY scheduled_for, occurrence_key"
    ))?;
    Ok(statement
        .query_map([], ScheduleOccurrence::from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Repository managing `schedules` and `schedule_occurrences`.
#[derive(Clone, Copy, Debug)]
pub struct ScheduleRepo<'a>(&'a Store);

impl<'a> ScheduleRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn insert_schedule(
        &self,
        input: NewSchedule<'_>,
        clock: &ScheduleClock,
    ) -> Result<Schedule> {
        let NewSchedule {
            companion,
            title,
            instruction,
            run_at,
            cron_expr,
        } = input;
        let timing = ScheduleTiming::from_fields(run_at, cron_expr)?;
        if title.chars().count() > 200 || instruction.chars().count() > 4_000 {
            return Err(StoreError::Invalid("schedule text is too long".into()));
        }
        let now = clock.now();
        let (run_at, cron_expr, first_at) = match timing {
            ScheduleTiming::Once(text) => (Some(text), None, parse_run_at(text, clock.zone())?),
            ScheduleTiming::Recurring(text) => {
                (None, Some(text), next_occurrence(text, now, clock.zone())?)
            }
        };
        let next_run_at = stamp(first_at);
        let id = uuid::Uuid::new_v4().to_string();
        self.0.write_one(
            &format!(
                "INSERT INTO schedules (id, companion_id, cron_expr, run_at, title, instruction, \
             enabled, next_run_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8) \
             RETURNING {SCHEDULE_FIELDS}"
            ),
            params![
                id,
                companion,
                cron_expr,
                run_at,
                title,
                instruction,
                next_run_at,
                stamp(now)
            ],
            Schedule::from_row,
        )
    }

    /// The enabled schedules of `companion`, for its `schedule_list` tool.
    pub fn schedules(&self, companion: &str) -> Result<Vec<Schedule>> {
        self.0.fetch_all(
            &format!(
                "SELECT {SCHEDULE_FIELDS} FROM schedules \
                 WHERE companion_id = ?1 AND enabled = 1 ORDER BY created_at, id"
            ),
            [companion],
            Schedule::from_row,
        )
    }

    /// Atomically materializes all due schedules for `ready_companions` into
    /// idempotent `schedule_occurrences` rows and returns pending occurrences.
    pub fn materialize_due_occurrences(
        &self,
        now: Timestamp,
        clock: &ScheduleClock,
        ready_companions: &std::collections::HashSet<String>,
    ) -> Result<Vec<ScheduleOccurrence>> {
        let now_stamp = stamp(now);
        self.0.write_tx(|tx| {
        let due = {
            let mut stmt = tx.prepare_cached(&format!(
                "SELECT {SCHEDULE_FIELDS} FROM schedules \
                 WHERE enabled = 1 AND next_run_at <= ?1 ORDER BY next_run_at, id"
            ))?;
            stmt.query_map([&now_stamp], Schedule::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for schedule in due {
            if !ready_companions.contains(&schedule.companion_id) {
                continue;
            }
            let scheduled_for = schedule.next_run_at.as_deref().ok_or_else(|| {
                StoreError::Storage("due schedule is missing its next firing time".into())
            })?;
            let occurrence_key = format!("schedule:{}:{scheduled_for}", schedule.id);
            let next_run_at = if schedule.one_shot() {
                None
            } else {
                match schedule
                    .cron_expr
                    .as_deref()
                    .map(|expression| next_occurrence(expression, now, clock.zone()))
                {
                    Some(Ok(at)) => Some(at),
                    Some(Err(error)) => {
                        tracing::warn!(%error, schedule = %schedule.id, "a repeating schedule exhausted its expression");
                        None
                    }
                    None => None,
                }
            };
            tx.prepare_cached(
                "INSERT OR IGNORE INTO schedule_occurrences \
                 (occurrence_key, schedule_id, companion_id, scheduled_for, title, instruction, \
                  status, attempts, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7)",
            )?
            .execute(params![
                occurrence_key,
                schedule.id,
                schedule.companion_id,
                scheduled_for,
                schedule.title,
                schedule.instruction,
                now_stamp,
            ])?;
            tx.prepare_cached(
                "UPDATE schedules SET last_triggered_at = ?2, next_run_at = ?3, enabled = ?4 \
                 WHERE id = ?1",
            )?
            .execute(params![
                schedule.id,
                now_stamp,
                next_run_at.map(stamp),
                if next_run_at.is_some() { 1 } else { 0 }
            ])?;
        }
        pending_occurrences(tx)
        })
    }

    /// Pending schedule occurrences awaiting confirmed delivery.
    pub fn pending_schedule_occurrences(&self) -> Result<Vec<ScheduleOccurrence>> {
        pending_occurrences(&self.0.conn())
    }

    /// Marks `occurrence_key` delivered once confirmed in the companion's conversation log.
    pub fn mark_occurrence_delivered(&self, occurrence_key: &str, at: Timestamp) -> Result<bool> {
        let changed = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE schedule_occurrences SET status = 'delivered', delivered_at = ?2, \
                 last_error = NULL WHERE occurrence_key = ?1 AND status != 'delivered'",
            )?
            .execute(params![occurrence_key, stamp(at)])?;
        Ok(changed > 0)
    }

    /// Records a failed delivery attempt on a pending occurrence.
    pub fn record_occurrence_attempt(&self, occurrence_key: &str, error: &str) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(
                "UPDATE schedule_occurrences SET attempts = attempts + 1, last_error = ?2 \
                 WHERE occurrence_key = ?1 AND status = 'pending'",
            )?
            .execute(params![occurrence_key, error])?;
        Ok(())
    }

    pub fn cancel_schedule(&self, companion: &str, id: &str) -> Result<bool> {
        self.0.write_tx(|tx| {
            let changed = tx
                .prepare_cached("DELETE FROM schedules WHERE id = ?1 AND companion_id = ?2")?
                .execute(params![id, companion])?;
            if changed > 0 {
                tx.prepare_cached(
                    "DELETE FROM schedule_occurrences WHERE schedule_id = ?1 AND status = 'pending'",
                )?
                .execute([id])?;
            }
            Ok(changed > 0)
        })
    }
}
