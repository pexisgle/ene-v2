//! Schedules: stored reminders and idempotent materialized occurrences.

#[cfg(test)]
mod tests;

use crate::{Store, StoreError, error::Result};
use jiff::{Timestamp, tz::TimeZone};
use rusqlite::params;

/// A literal five-field cron expression, without seconds, years or nicknames.
#[derive(Clone, Debug, PartialEq)]
pub struct Cron {
    parsed: croner::Cron,
}

impl Eq for Cron {}

impl Cron {
    /// Parses a five-field expression. An invalid expression is a request
    /// error, not an empty schedule.
    pub fn parse(expression: &str) -> Result<Self> {
        if expression.chars().count() > 256 {
            return Err(StoreError::Invalid("cron_expr is too long".into()));
        }
        if expression.contains('@') {
            return Err(StoreError::Invalid(
                "cron_expr must use 5 literal fields, not a nickname".into(),
            ));
        }
        let parsed = croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Disallowed)
            .year(croner::parser::Year::Disallowed)
            .build()
            .parse(expression)
            .map_err(|error| StoreError::Invalid(format!("cron_expr is invalid: {error}")))?;
        Ok(Self { parsed })
    }

    fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        self.parsed
            .find_next_occurrence(&after.to_zoned(zone.clone()), false)
            .ok()
            .map(|next| next.timestamp())
    }
}

/// The first time the five-field `expression` matches after `after`, read on
/// the wall clock of `zone`.
pub fn next_occurrence(expression: &str, after: Timestamp, zone: &TimeZone) -> Result<Timestamp> {
    Cron::parse(expression)?
        .next_after(after, zone)
        .ok_or_else(|| {
            StoreError::Invalid("cron_expr names no time within the search window".into())
        })
}

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

fn parse_run_at(text: &str, zone: &TimeZone) -> Result<Timestamp> {
    let text = text.trim();
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    let local: jiff::civil::DateTime = text.parse().map_err(|_| {
        StoreError::Invalid("run_at must be an RFC 3339 time or a local date and time".into())
    })?;
    if !text.chars().any(|c| matches!(c, 'T' | 't' | ' ')) {
        return Err(StoreError::Invalid(
            "run_at must include a time of day".into(),
        ));
    }
    zone.to_zoned(local)
        .map(|zoned| zoned.timestamp())
        .map_err(|_| StoreError::Invalid("run_at is outside the supported range".into()))
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
        let run_at = run_at.filter(|value| !value.trim().is_empty());
        let cron_expr = cron_expr.filter(|value| !value.trim().is_empty());
        match (run_at, cron_expr) {
            (None, None) => {
                return Err(StoreError::Invalid(
                    "a schedule needs exactly one of run_at or cron_expr".into(),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(StoreError::Invalid(
                    "run_at and cron_expr are mutually exclusive".into(),
                ));
            }
            _ => {}
        }
        if title.chars().count() > 200 || instruction.chars().count() > 4_000 {
            return Err(StoreError::Invalid("schedule text is too long".into()));
        }
        let now = clock.now();
        let next_run_at = match (run_at, cron_expr) {
            (Some(run_at), None) => stamp(parse_run_at(run_at, clock.zone())?),
            (None, Some(cron_expr)) => stamp(next_occurrence(cron_expr, now, clock.zone())?),
            _ => {
                return Err(StoreError::Invalid(
                    "a schedule needs exactly one of run_at or cron_expr".into(),
                ));
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        self.0.write_one(
            "INSERT INTO schedules (id, companion_id, cron_expr, run_at, title, instruction, \
             enabled, next_run_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8) \
             RETURNING id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             next_run_at, created_at, last_triggered_at",
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
            row_to_schedule,
        )
    }

    /// The enabled schedules of `companion`, for its `schedule_list` tool.
    pub fn schedules(&self, companion: &str) -> Result<Vec<Schedule>> {
        self.0.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             next_run_at, created_at, last_triggered_at FROM schedules \
             WHERE companion_id = ?1 AND enabled = 1 ORDER BY created_at, id",
            [companion],
            row_to_schedule,
        )
    }

    /// The enabled schedules due at `now`.
    pub fn due_schedules(&self, now: Timestamp) -> Result<Vec<Schedule>> {
        self.0.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             next_run_at, created_at, last_triggered_at FROM schedules \
             WHERE enabled = 1 AND next_run_at <= ?1 ORDER BY next_run_at, id",
            [stamp(now)],
            row_to_schedule,
        )
    }

    /// Records that `id` fired at `at` and moves it to `next_run_at`.
    pub fn mark_schedule_fired(
        &self,
        id: &str,
        at: Timestamp,
        next_run_at: Option<Timestamp>,
    ) -> Result<bool> {
        let changed = self
            .0
            .conn()
            .prepare_cached(
                "UPDATE schedules SET last_triggered_at = ?2, next_run_at = ?3, enabled = ?4 \
                 WHERE id = ?1",
            )?
            .execute(params![
                id,
                stamp(at),
                next_run_at.map(stamp),
                if next_run_at.is_some() { 1 } else { 0 }
            ])?;
        Ok(changed > 0)
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
            let mut stmt = tx.prepare_cached(
                "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
                 next_run_at, created_at, last_triggered_at FROM schedules \
                 WHERE enabled = 1 AND next_run_at <= ?1 ORDER BY next_run_at, id",
            )?;
            stmt.query_map([&now_stamp], row_to_schedule)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for schedule in due {
            if !ready_companions.contains(&schedule.companion_id) {
                continue;
            }
            let scheduled_for = schedule
                .next_run_at
                .clone()
                .unwrap_or_else(|| now_stamp.clone());
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
        let pending = {
            let mut stmt = tx.prepare_cached(
                "SELECT occurrence_key, schedule_id, companion_id, scheduled_for, title, \
                 instruction, status, attempts, last_error, created_at, delivered_at \
                 FROM schedule_occurrences WHERE status = 'pending' \
                 ORDER BY scheduled_for, occurrence_key",
            )?;
            stmt.query_map([], row_to_occurrence)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(pending)
        })
    }

    /// Pending schedule occurrences awaiting confirmed delivery.
    pub fn pending_schedule_occurrences(&self) -> Result<Vec<ScheduleOccurrence>> {
        self.0.fetch_all(
            "SELECT occurrence_key, schedule_id, companion_id, scheduled_for, title, \
             instruction, status, attempts, last_error, created_at, delivered_at \
             FROM schedule_occurrences WHERE status = 'pending' \
             ORDER BY scheduled_for, occurrence_key",
            [],
            row_to_occurrence,
        )
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

fn row_to_occurrence(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleOccurrence> {
    Ok(ScheduleOccurrence {
        occurrence_key: row.get("occurrence_key")?,
        schedule_id: row.get("schedule_id")?,
        companion_id: row.get("companion_id")?,
        scheduled_for: row.get("scheduled_for")?,
        title: row.get("title")?,
        instruction: row.get("instruction")?,
        status: row.get("status")?,
        attempts: row.get("attempts")?,
        last_error: row.get("last_error")?,
        created_at: row.get("created_at")?,
        delivered_at: row.get("delivered_at")?,
    })
}

fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get("id")?,
        companion_id: row.get("companion_id")?,
        title: row.get("title")?,
        instruction: row.get("instruction")?,
        run_at: row.get("run_at")?,
        cron_expr: row.get("cron_expr")?,
        enabled: row.get("enabled")?,
        next_run_at: row.get("next_run_at")?,
        created_at: row.get("created_at")?,
        last_triggered_at: row.get("last_triggered_at")?,
    })
}
