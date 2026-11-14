//! Schedules: stored reminders for the head. The storage validates what a
//! tool may register and resolves when every row next fires against the
//! host's wall clock, so the firing loop only reads the stored `next_run_at`
//! instead of parsing a cron expression on every poll
//! (docs/features/autonomy/schedule.md).
use super::{Result, Store};
use crate::host::error::HostError;
use jiff::{Timestamp, tz::TimeZone};
use rusqlite::params;

/// A five-field cron expression. A six-field one counts seconds and a
/// seven-field one names years, so both are refused before croner reads them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cron {
    parsed: croner::Cron,
}

impl Eq for Cron {}

impl Cron {
    /// Parses a five-field expression. An invalid expression is a request
    /// error, not an empty schedule.
    pub(crate) fn parse(expression: &str) -> Result<Self> {
        if expression.chars().count() > 256 {
            return Err(HostError::invalid("cron_expr is too long"));
        }
        let fields = expression.split_whitespace().count();
        if fields != 5 {
            return Err(HostError::invalid(format!(
                "cron_expr must have exactly 5 fields \
                 (minute hour day-of-month month day-of-week), got {fields}"
            )));
        }
        let parsed = expression
            .parse::<croner::Cron>()
            .map_err(|error| HostError::invalid(format!("cron_expr is invalid: {error}")))?;
        Ok(Self { parsed })
    }

    /// The first matching instant strictly after `after`, read on the civil
    /// clock of `zone`, or `None` when croner cannot find a matching time.
    ///
    /// Croner searches the wall clock natively. A time inside a daylight-saving
    /// gap never happens, so the firing lands on the instant the clock jumps;
    /// a repeated time resolves to its earlier occurrence. The host does no
    /// conversion, retry or gap loop of its own.
    fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        self.parsed
            .find_next_occurrence(&after.to_zoned(zone.clone()), false)
            .ok()
            .map(|next| next.timestamp())
    }
}

/// The first time the five-field `expression` matches after `after`, read on
/// the wall clock of `zone`. An invalid expression, or one that names no time
/// with no next occurrence, is an invalid request.
pub(crate) fn next_occurrence(
    expression: &str,
    after: Timestamp,
    zone: &TimeZone,
) -> Result<Timestamp> {
    Cron::parse(expression)?
        .next_after(after, zone)
        .ok_or_else(|| HostError::invalid("cron_expr names no time within the search window"))
}

/// The wall clock schedules are read against: the host's local time zone and
/// the current instant. Tests freeze the instant so a firing is deterministic.
#[derive(Clone, Debug, bevy_ecs::prelude::Resource)]
pub(crate) struct ScheduleClock {
    zone: TimeZone,
    frozen: Option<Timestamp>,
    /// The system named no time zone, so schedules are read in UTC.
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
    pub(crate) fn system() -> Self {
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

    /// A clock frozen at `now` in `zone`, for tests.
    #[cfg(test)]
    pub(crate) fn fixed(zone: TimeZone, now: Timestamp) -> Self {
        Self {
            zone,
            frozen: Some(now),
            fallback: false,
        }
    }

    pub(crate) fn zone(&self) -> &TimeZone {
        &self.zone
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.frozen.unwrap_or_else(Timestamp::now)
    }

    /// How the zone is named to the head in the tool results.
    pub(crate) fn name(&self) -> &str {
        if self.fallback {
            "UTC (system time zone unavailable)"
        } else {
            self.zone.iana_name().unwrap_or("local")
        }
    }
}

/// The canonical stored text of an instant: UTC, whole seconds and `Z`, so
/// SQLite string order is time order. A sub-second `run_at` fires on its
/// whole second.
fn stamp(at: Timestamp) -> String {
    format!("{at:.0}")
}

/// Reads `run_at` on the host's wall clock. A value with an offset names an
/// instant; a civil value without one is a local date and time in `zone`.
fn parse_run_at(text: &str, zone: &TimeZone) -> Result<Timestamp> {
    let text = text.trim();
    // An explicit offset or `Z` is absolute and needs no zone.
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    let local: jiff::civil::DateTime = text.parse().map_err(|_| {
        HostError::invalid("run_at must be an RFC 3339 time or a local date and time")
    })?;
    // A bare date parses as midnight; the head has to say when on that day.
    if !text.chars().any(|c| matches!(c, 'T' | 't' | ' ')) {
        return Err(HostError::invalid("run_at must include a time of day"));
    }
    zone.to_zoned(local)
        .map(|zoned| zoned.timestamp())
        .map_err(|_| HostError::invalid("run_at is outside the supported range"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub(crate) id: String,
    pub(crate) companion_id: String,
    pub(crate) title: String,
    pub(crate) instruction: String,
    pub(crate) run_at: Option<String>,
    pub(crate) cron_expr: Option<String>,
    pub(crate) enabled: bool,
    /// The next firing, UTC RFC 3339; `None` once a one-shot has fired.
    pub(crate) next_run_at: Option<String>,
    pub(crate) created_at: String,
    pub(crate) last_triggered_at: Option<String>,
}

impl Schedule {
    /// Whether the row fires once and is then disabled.
    pub(crate) fn one_shot(&self) -> bool {
        self.run_at.is_some()
    }
}

impl Store {
    pub(crate) fn insert_schedule(
        &self,
        companion: &str,
        title: &str,
        instruction: &str,
        run_at: Option<&str>,
        cron_expr: Option<&str>,
        clock: &ScheduleClock,
    ) -> Result<Schedule> {
        // A schedule fires once or repeats, never both and never neither.
        let run_at = run_at.filter(|value| !value.trim().is_empty());
        let cron_expr = cron_expr.filter(|value| !value.trim().is_empty());
        match (run_at, cron_expr) {
            (None, None) => {
                return Err(HostError::invalid(
                    "a schedule needs exactly one of run_at or cron_expr",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(HostError::invalid(
                    "run_at and cron_expr are mutually exclusive",
                ));
            }
            _ => {}
        }
        if title.chars().count() > 200 || instruction.chars().count() > 4_000 {
            return Err(HostError::invalid("schedule text is too long"));
        }
        let now = clock.now();
        // The first firing is resolved here, against the host's wall clock,
        // so a poll never parses or computes it again.
        let next_run_at = match (run_at, cron_expr) {
            (Some(run_at), None) => stamp(parse_run_at(run_at, clock.zone())?),
            (None, Some(cron_expr)) => stamp(next_occurrence(cron_expr, now, clock.zone())?),
            _ => {
                return Err(HostError::invalid(
                    "a schedule needs exactly one of run_at or cron_expr",
                ));
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        self.write_one(
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
    pub(crate) fn schedules(&self, companion: &str) -> Result<Vec<Schedule>> {
        self.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             next_run_at, created_at, last_triggered_at FROM schedules \
             WHERE companion_id = ?1 AND enabled = 1 ORDER BY created_at, id",
            [companion],
            row_to_schedule,
        )
    }

    /// The enabled schedules due at `now`, for the firing loop. The indexed
    /// comparison is over the canonical whole-second timestamps, so string
    /// order is time order.
    pub(crate) fn due_schedules(&self, now: Timestamp) -> Result<Vec<Schedule>> {
        self.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             next_run_at, created_at, last_triggered_at FROM schedules \
             WHERE enabled = 1 AND next_run_at <= ?1 ORDER BY next_run_at, id",
            [stamp(now)],
            row_to_schedule,
        )
    }

    /// Records that `id` fired at `at` and moves it to `next_run_at`. A
    /// missing next time disables the row, which is how a one-shot ends.
    /// `false` when the row no longer exists.
    pub(crate) fn mark_schedule_fired(
        &self,
        id: &str,
        at: Timestamp,
        next_run_at: Option<Timestamp>,
    ) -> Result<bool> {
        let changed = self.conn()?.execute(
            "UPDATE schedules SET last_triggered_at = ?2, next_run_at = ?3, enabled = ?4 \
             WHERE id = ?1",
            params![
                id,
                stamp(at),
                next_run_at.map(stamp),
                if next_run_at.is_some() { 1 } else { 0 }
            ],
        )?;
        Ok(changed > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    /// A UTC instant at whole-second precision.
    fn utc(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
        TimeZone::UTC
            .to_zoned(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
            .timestamp()
    }

    fn utc_clock() -> ScheduleClock {
        ScheduleClock::fixed(TimeZone::UTC, utc(2026, 11, 14, 8, 0))
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    #[test]
    fn a_cron_expression_must_name_exactly_five_fields() {
        for expression in ["", "* * * *", "* * * * * *", "* * * * * * *", "not a cron"] {
            assert!(Cron::parse(expression).is_err(), "{expression}");
        }
        // The length cap is the host's own bound, not the parser's.
        assert!(Cron::parse(&"* ".repeat(200)).is_err());
        assert!(Cron::parse("* * * * *").is_ok());
        assert!(Cron::parse("0 9 * * 1-5").is_ok());
        let store = store();
        let clock = utc_clock();
        assert!(matches!(
            store.insert_schedule("ene", "t", "i", None, Some("0 9 * *"), &clock),
            Err(HostError::Invalid(_))
        ));
    }

    #[test]
    fn a_schedule_takes_exactly_one_valid_time() {
        let store = store();
        let clock = utc_clock();
        // Neither or both is refused.
        assert!(matches!(
            store.insert_schedule("ene", "t", "i", None, None, &clock),
            Err(HostError::Invalid(_))
        ));
        assert!(matches!(
            store.insert_schedule(
                "ene",
                "t",
                "i",
                Some("2030-01-01T09:00:00Z"),
                Some("0 9 * * *"),
                &clock
            ),
            Err(HostError::Invalid(_))
        ));
        // A one-shot time must be an absolute or a local date and time.
        for invalid in ["tomorrow", "2030-02-30T09:00:00", "2030-01-01"] {
            assert!(
                matches!(
                    store.insert_schedule("ene", "t", "i", Some(invalid), None, &clock),
                    Err(HostError::Invalid(_))
                ),
                "{invalid}"
            );
        }
        let absolute = store
            .insert_schedule("ene", "t", "i", Some("2030-01-01T09:00:00Z"), None, &clock)
            .unwrap();
        assert_eq!(
            absolute.next_run_at.as_deref(),
            Some("2030-01-01T09:00:00Z")
        );
        // A value without an offset is read on the host's wall clock.
        let local = store
            .insert_schedule("ene", "t", "i", Some("2030-01-01T09:00:00"), None, &clock)
            .unwrap();
        assert_eq!(local.next_run_at.as_deref(), Some("2030-01-01T09:00:00Z"));
        assert!(
            store
                .insert_schedule("ene", "t", "i", None, Some("* * * * *"), &clock)
                .is_ok()
        );
    }

    #[test]
    fn a_local_run_at_is_read_in_the_host_time_zone_not_utc() {
        let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
        // 09:00 in Tokyo on 2026-11-13.
        let clock = ScheduleClock::fixed(tokyo.clone(), utc(2026, 11, 13, 0, 0));
        let store = store();
        let schedule = store
            .insert_schedule("ene", "t", "i", Some("2026-11-15T09:00:00"), None, &clock)
            .unwrap();
        assert_eq!(
            schedule.next_run_at.as_deref(),
            Some("2026-11-15T00:00:00Z"),
            "local 09:00 is 00:00 UTC in Tokyo"
        );
        // An explicit offset is still absolute.
        let schedule = store
            .insert_schedule(
                "ene",
                "t",
                "i",
                Some("2026-11-15T09:00:00+09:00"),
                None,
                &clock,
            )
            .unwrap();
        assert_eq!(
            schedule.next_run_at.as_deref(),
            Some("2026-11-15T00:00:00Z")
        );
    }

    #[test]
    fn firing_a_row_advances_or_disables_it() {
        let store = store();
        let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
        // 08:00 on 2026-11-14 in Tokyo.
        let clock = ScheduleClock::fixed(tokyo, utc(2026, 11, 13, 23, 0));
        let one_shot = store
            .insert_schedule(
                "ene",
                "reminder",
                "say it",
                Some("2000-01-01T00:00:00Z"),
                None,
                &clock,
            )
            .unwrap();
        let repeating = store
            .insert_schedule("ene", "daily", "say it", None, Some("0 9 * * *"), &clock)
            .unwrap();
        assert_eq!(
            repeating.next_run_at.as_deref(),
            Some("2026-11-14T00:00:00Z")
        );
        // Only the overdue one-shot is due before Tokyo 09:00.
        let due = store.due_schedules(clock.now()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, one_shot.id);
        assert_eq!(store.schedules("ene").unwrap().len(), 2);

        // Firing the one-shot disables it and clears its next time.
        assert!(
            store
                .mark_schedule_fired(&one_shot.id, clock.now(), None)
                .unwrap()
        );
        assert!(
            !store
                .mark_schedule_fired("missing", clock.now(), None)
                .unwrap()
        );
        let (enabled, last, next) = stored(&store, &one_shot.id);
        assert!(!enabled, "a one-shot row is disabled after firing");
        assert_eq!(last.as_deref(), Some("2026-11-13T23:00:00Z"));
        assert_eq!(next, None);

        // The repeating row is due at Tokyo 09:00 and stays enabled.
        let fired_at = utc(2026, 11, 14, 0, 0);
        let due = store.due_schedules(fired_at).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, repeating.id);
        assert!(
            store
                .mark_schedule_fired(&repeating.id, fired_at, Some(utc(2026, 11, 15, 0, 0)))
                .unwrap()
        );
        let (enabled, last, next) = stored(&store, &repeating.id);
        assert!(enabled, "a repeating row stays enabled");
        assert_eq!(last.as_deref(), Some("2026-11-14T00:00:00Z"));
        assert_eq!(next.as_deref(), Some("2026-11-15T00:00:00Z"));
        // The same occurrence never fires twice; the next day's does.
        assert!(store.due_schedules(fired_at).unwrap().is_empty());
        assert_eq!(
            store.due_schedules(utc(2026, 11, 15, 0, 0)).unwrap().len(),
            1
        );

        assert!(store.cancel_schedule("ene", &repeating.id).unwrap());
        assert!(
            store
                .due_schedules(utc(2026, 11, 16, 0, 0))
                .unwrap()
                .is_empty()
        );
    }

    fn stored(store: &Store, id: &str) -> (bool, Option<String>, Option<String>) {
        store
            .conn()
            .unwrap()
            .query_row(
                "SELECT enabled, last_triggered_at, next_run_at FROM schedules WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }
}
