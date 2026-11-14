//! Schedules: stored reminders for the head. The storage validates what a
//! tool may register, and the firing check answers whether a stored row is
//! due and when a repeating one comes due again
//! (docs/features/autonomy/schedule.md).
use super::{Result, Store, epoch, now};
use crate::host::error::HostError;
use rusqlite::params;
use time::{Date, Duration, OffsetDateTime, UtcOffset};

/// The inclusive bounds of a five-field cron expression, field by field:
/// minute, hour, day of month, month, day of week (0 and 7 are Sunday).
const CRON_LIMITS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

/// How far ahead a repeating expression is searched, in days. A leap day can
/// be eight years away (2100 is not a leap year), so this is the smallest
/// bound that still finds every yearly date an expression can name.
const SEARCH_DAYS: u64 = 366 * 9;

/// Validates one field of a cron expression: `*`, a value, a `first-last`
/// range, a comma-separated list of them, and an optional `/step` on any of
/// them. Names and 6-field (seconds) expressions are refused.
fn check_cron_field(field: &str, min: u32, max: u32) -> Result<()> {
    if field.is_empty() {
        return Err(HostError::invalid("cron_expr has an empty field"));
    }
    for item in field.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (item, None),
        };
        if let Some(step) = step {
            let step = step
                .parse::<u32>()
                .map_err(|_| HostError::invalid("cron_expr has a non-numeric step"))?;
            if step == 0 || step > max.saturating_sub(min).saturating_add(1) {
                return Err(HostError::invalid("cron_expr has an out-of-range step"));
            }
        }
        if range == "*" {
            continue;
        }
        let parse = |value: &str| {
            value
                .parse::<u32>()
                .map_err(|_| HostError::invalid("cron_expr has a non-numeric value"))
        };
        let (first, last) = match range.split_once('-') {
            Some((first, last)) => (parse(first)?, parse(last)?),
            None => {
                let value = parse(range)?;
                (value, value)
            }
        };
        if first < min || last > max || first > last {
            return Err(HostError::invalid(
                "cron_expr has a value outside its field's range",
            ));
        }
    }
    Ok(())
}

/// Validates a five-field cron expression with valid ranges.
fn check_cron(expression: &str) -> Result<()> {
    if expression.chars().count() > 256 {
        return Err(HostError::invalid("cron_expr is too long"));
    }
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(HostError::invalid(
            "cron_expr must have exactly 5 fields (minute hour day-of-month month day-of-week)",
        ));
    }
    for (field, (min, max)) in fields.iter().zip(CRON_LIMITS) {
        check_cron_field(field, min, max)?;
    }
    Ok(())
}

/// A validated five-field cron expression, resolved into the values each
/// field matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cron {
    minute: [bool; 60],
    hour: [bool; 24],
    /// Indexed by day of month, 1..=31.
    day: [bool; 32],
    /// Indexed by month, 1..=12.
    month: [bool; 13],
    /// Indexed by day of week, 0 (Sunday)..=7 (Sunday again).
    weekday: [bool; 8],
    /// Whether the field was written `*`: cron's day-of-month/day-of-week
    /// rule matches either field when both are restricted and both when one
    /// is not.
    day_any: bool,
    weekday_any: bool,
}

/// Fills `bits` with the values one comma-separated item of a field names.
fn parse_cron_item<const N: usize>(
    item: &str,
    min: u32,
    max: u32,
    bits: &mut [bool; N],
) -> Option<()> {
    let (range, step) = match item.split_once('/') {
        Some((range, step)) => (range, Some(step.parse::<u32>().ok()?)),
        None => (item, None),
    };
    if step == Some(0) {
        return None;
    }
    let parse = |value: &str| value.parse::<u32>().ok();
    let (first, last) = if range == "*" {
        (min, max)
    } else if let Some((first, last)) = range.split_once('-') {
        (parse(first)?, parse(last)?)
    } else {
        let value = parse(range)?;
        // `a/step` runs from `a` to the field's end, as cron does.
        (value, if step.is_some() { max } else { value })
    };
    if first < min || last > max || first > last {
        return None;
    }
    let step = step.unwrap_or(1);
    let mut value = first;
    while value <= last {
        *bits.get_mut(value as usize)? = true;
        value = value.checked_add(step)?;
    }
    Some(())
}

impl Cron {
    /// Parses a validated expression. `None` means the expression is invalid
    /// or that its fields do not fit their bounds, so nothing matches.
    pub(crate) fn parse(expression: &str) -> Option<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = <[&str; 5]>::try_from(fields).ok()?;
        let mut cron = Self {
            minute: [false; 60],
            hour: [false; 24],
            day: [false; 32],
            month: [false; 13],
            weekday: [false; 8],
            day_any: day.starts_with('*'),
            weekday_any: weekday.starts_with('*'),
        };
        for (index, field) in [minute, hour, day, month, weekday].into_iter().enumerate() {
            let (min, max) = CRON_LIMITS[index];
            for item in field.split(',') {
                match index {
                    0 => parse_cron_item(item, min, max, &mut cron.minute)?,
                    1 => parse_cron_item(item, min, max, &mut cron.hour)?,
                    2 => parse_cron_item(item, min, max, &mut cron.day)?,
                    3 => parse_cron_item(item, min, max, &mut cron.month)?,
                    _ => parse_cron_item(item, min, max, &mut cron.weekday)?,
                }
            }
        }
        // 7 is Sunday exactly as 0 is.
        if cron.weekday[7] {
            cron.weekday[0] = true;
        }
        Some(cron)
    }

    /// Whether the fields name `date`, by cron's day-of-month/day-of-week
    /// rule: when both are restricted, either may match.
    fn matches_date(&self, date: Date) -> bool {
        let month = date.month() as usize;
        if !self.month[month] {
            return false;
        }
        let day = date.day() as usize;
        let weekday = date.weekday().number_days_from_sunday() as usize;
        let day_match = self.day[day];
        let weekday_match = self.weekday[weekday];
        if self.day_any || self.weekday_any {
            day_match && weekday_match
        } else {
            day_match || weekday_match
        }
    }

    /// The first matching time strictly after `after`, in seconds since the
    /// Unix epoch, or `None` when no matching time is within
    /// [`SEARCH_DAYS`].
    fn next_after(&self, after: i64) -> Option<i64> {
        // The first whole minute after `after`: cron's granularity is one
        // minute, so a matching minute is never fired twice.
        let start = after.div_euclid(60).checked_add(1)?.checked_mul(60)?;
        let first = OffsetDateTime::from_unix_timestamp(start)
            .ok()?
            .to_offset(UtcOffset::UTC);
        for day in 0..SEARCH_DAYS {
            let date = first
                .date()
                .checked_add(Duration::days(i64::try_from(day).ok()?))?;
            if !self.matches_date(date) {
                continue;
            }
            for hour in 0..24u8 {
                if !self.hour[hour as usize] {
                    continue;
                }
                for minute in 0..60u8 {
                    if !self.minute[minute as usize] {
                        continue;
                    }
                    let at = date
                        .with_hms(hour, minute, 0)
                        .ok()?
                        .assume_utc()
                        .unix_timestamp();
                    if at >= start {
                        return Some(at);
                    }
                }
            }
        }
        None
    }
}

/// The first time the five-field `expression` matches after the Unix time
/// `after`, in seconds since the epoch. `None` when the expression is
/// invalid or names no time within [`SEARCH_DAYS`].
pub(crate) fn next_occurrence(expression: &str, after: i64) -> Option<i64> {
    Cron::parse(expression)?.next_after(after)
}

/// Whether the five-field `expression` matches the Unix time `at` (in UTC).
#[cfg(test)]
pub(crate) fn cron_matches(expression: &str, at: i64) -> bool {
    let Some(cron) = Cron::parse(expression) else {
        return false;
    };
    let Some(time) = OffsetDateTime::from_unix_timestamp(at).ok() else {
        return false;
    };
    let time = time.to_offset(UtcOffset::UTC);
    cron.matches_date(time.date())
        && cron.hour[usize::from(time.hour())]
        && cron.minute[usize::from(time.minute())]
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
    pub(crate) created_at: String,
    pub(crate) last_triggered_at: Option<String>,
}

impl Schedule {
    /// Whether this row fires at Unix time `now`: a one-shot row at or past
    /// its `run_at` and never fired, a repeating one at or past its first
    /// occurrence after the last firing (or its creation).
    pub(crate) fn due(&self, now: i64) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(run_at) = &self.run_at {
            return self.last_triggered_at.is_none() && epoch(run_at).is_some_and(|at| at <= now);
        }
        let Some(expression) = &self.cron_expr else {
            return false;
        };
        let base = self
            .last_triggered_at
            .as_deref()
            .and_then(epoch)
            .or_else(|| epoch(&self.created_at));
        base.is_some_and(|base| next_occurrence(expression, base).is_some_and(|at| at <= now))
    }

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
        if let Some(run_at) = run_at
            && super::epoch(run_at).is_none()
        {
            return Err(HostError::invalid("run_at must be an RFC 3339 time"));
        }
        if let Some(cron_expr) = cron_expr {
            check_cron(cron_expr)?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.write_one(
            "INSERT INTO schedules (id, companion_id, cron_expr, run_at, title, instruction, \
             enabled, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7) \
             RETURNING id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             created_at, last_triggered_at",
            params![id, companion, cron_expr, run_at, title, instruction, now()],
            row_to_schedule,
        )
    }

    /// The enabled schedules of `companion`, for its `schedule_list` tool.
    pub(crate) fn schedules(&self, companion: &str) -> Result<Vec<Schedule>> {
        self.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             created_at, last_triggered_at FROM schedules \
             WHERE companion_id = ?1 AND enabled = 1 ORDER BY created_at, id",
            [companion],
            row_to_schedule,
        )
    }

    /// Every enabled schedule of every companion, for the firing loop.
    pub(crate) fn enabled_schedules(&self) -> Result<Vec<Schedule>> {
        self.fetch_all(
            "SELECT id, companion_id, title, instruction, run_at, cron_expr, enabled, \
             created_at, last_triggered_at FROM schedules WHERE enabled = 1 \
             ORDER BY created_at, id",
            [],
            row_to_schedule,
        )
    }

    /// Records that `id` fired at `at`; a one-shot row is disabled, a
    /// repeating one stays enabled for its next occurrence. `false` when the
    /// row no longer exists.
    pub(crate) fn mark_schedule_fired(&self, id: &str, at: &str, one_shot: bool) -> Result<bool> {
        let changed = self.conn()?.execute(
            "UPDATE schedules SET last_triggered_at = ?2, enabled = ?3 WHERE id = ?1",
            params![id, at, if one_shot { 0 } else { 1 }],
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
        created_at: row.get("created_at")?,
        last_triggered_at: row.get("last_triggered_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The Unix time of a UTC moment.
    fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> i64 {
        let date = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day)
            .unwrap();
        date.with_hms(hour, minute, 0)
            .unwrap()
            .assume_utc()
            .unix_timestamp()
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    #[test]
    fn a_schedule_takes_exactly_one_valid_time() {
        let store = store();
        // Neither or both is refused.
        assert!(matches!(
            store.insert_schedule("ene", "t", "i", None, None),
            Err(HostError::Invalid(_))
        ));
        assert!(matches!(
            store.insert_schedule(
                "ene",
                "t",
                "i",
                Some("2030-01-01T09:00:00Z"),
                Some("0 9 * * *")
            ),
            Err(HostError::Invalid(_))
        ));
        // A one-shot time must be RFC 3339.
        assert!(matches!(
            store.insert_schedule("ene", "t", "i", Some("tomorrow"), None),
            Err(HostError::Invalid(_))
        ));
        assert!(
            store
                .insert_schedule("ene", "t", "i", Some("2030-01-01T09:00:00Z"), None)
                .is_ok()
        );
        assert!(
            store
                .insert_schedule("ene", "t", "i", None, Some("* * * * *"))
                .is_ok()
        );
    }

    #[test]
    fn cron_expressions_need_five_fields_with_valid_ranges() {
        for expression in [
            "* * * * *",
            "0 9 * * 1-5",
            "*/15 8-18 * * *",
            "0 0 1,15 * *",
            "5/10 * * * 0",
            "0 0 * * 7",
        ] {
            assert!(check_cron(expression).is_ok(), "{expression}");
        }
        for expression in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "1-0 * * * *",
            "a * * * *",
            "-1 * * * *",
            "1,,2 * * * *",
        ] {
            assert!(check_cron(expression).is_err(), "{expression}");
        }
        let store = store();
        assert!(matches!(
            store.insert_schedule("ene", "t", "i", None, Some("0 9 * *")),
            Err(HostError::Invalid(_))
        ));
    }

    #[test]
    fn cron_values_resolve_and_weekday_seven_is_sunday() {
        // 2026-11-15 is a Sunday, 2026-11-16 a Monday.
        assert!(cron_matches("0 9 * * 7", at(2026, 11, 15, 9, 0)));
        assert!(cron_matches("0 9 * * 0", at(2026, 11, 15, 9, 0)));
        assert!(!cron_matches("0 9 * * 1-5", at(2026, 11, 15, 9, 0)));
        assert!(cron_matches("*/15 * * * *", at(2026, 11, 15, 9, 45)));
        assert!(!cron_matches("*/15 * * * *", at(2026, 11, 15, 9, 46)));
        assert!(cron_matches("5/10 * * * *", at(2026, 11, 15, 9, 25)));
        assert!(!cron_matches("5/10 * * * *", at(2026, 11, 15, 9, 26)));
        assert!(cron_matches("0 0 1,15 * *", at(2026, 11, 15, 0, 0)));
        // Both day fields restricted: either may match (13th or Friday).
        let friday_the_13th = at(2026, 11, 13, 0, 0);
        assert!(cron_matches("0 0 13 * 5", friday_the_13th));
        let ordinary_day = at(2026, 11, 12, 0, 0);
        assert!(!cron_matches("0 0 13 * 5", ordinary_day));
        // Day of month restricted, weekday `*`: only the day matters.
        assert!(cron_matches("0 0 12 * *", ordinary_day));
        assert!(!cron_matches("0 0 13 * *", ordinary_day));
        // Weekday restricted, day `*`: only the weekday matters.
        assert!(cron_matches("0 0 * * 4", ordinary_day));
        assert!(!cron_matches("0 0 * * 5", ordinary_day));
        // A day field written with a leading `*` is unrestricted for cron's
        // rule, so the other day field still has to match.
        let friday = at(2026, 11, 13, 0, 0);
        assert!(cron_matches("0 0 */1 * 5", friday));
        assert!(!cron_matches("0 0 */1 * 5", ordinary_day));
        assert!(cron_matches("0 0 13 * */1", friday));
        assert!(!cron_matches("0 0 12 * */1", friday));
    }

    #[test]
    fn the_next_occurrence_is_strictly_after_the_given_time() {
        let after = at(2026, 11, 14, 9, 18);
        assert_eq!(
            next_occurrence("* * * * *", after),
            Some(at(2026, 11, 14, 9, 19))
        );
        // The same minute does not fire twice: at 09:19:00 the next is 09:20.
        assert_eq!(
            next_occurrence("19 9 * * *", at(2026, 11, 14, 9, 19)),
            Some(at(2026, 11, 15, 9, 19))
        );
        // A weekday expression skips the weekend: Friday 10:00 -> Monday 09:00.
        assert_eq!(
            next_occurrence("0 9 * * 1-5", at(2026, 11, 13, 10, 0)),
            Some(at(2026, 11, 16, 9, 0))
        );
        assert_eq!(
            next_occurrence("*/15 * * * *", at(2026, 11, 14, 9, 18)),
            Some(at(2026, 11, 14, 9, 30))
        );
        assert_eq!(
            next_occurrence("0 0 1 1 *", at(2026, 11, 14, 9, 18)),
            Some(at(2027, 1, 1, 0, 0))
        );
        // A leap day is found although 2100 is not a leap year.
        assert_eq!(
            next_occurrence("0 0 29 2 *", at(2096, 3, 1, 0, 0)),
            Some(at(2104, 2, 29, 0, 0))
        );
        // A date that never comes has no occurrence.
        assert_eq!(next_occurrence("0 0 31 2 *", at(2026, 11, 14, 9, 18)), None);
        assert_eq!(next_occurrence("not a cron", after), None);
    }

    #[test]
    fn due_answers_for_one_shot_and_repeating_rows() {
        let one_shot = Schedule {
            id: "a".into(),
            companion_id: "ene".into(),
            title: "t".into(),
            instruction: "i".into(),
            run_at: Some("2026-11-14T09:00:00Z".into()),
            cron_expr: None,
            enabled: true,
            created_at: "2026-11-14T08:00:00Z".into(),
            last_triggered_at: None,
        };
        let now = at(2026, 11, 14, 9, 0);
        assert!(one_shot.due(now));
        assert!(one_shot.one_shot());
        assert!(!one_shot.due(now - 1));
        let fired = Schedule {
            last_triggered_at: Some("2026-11-14T09:00:00Z".into()),
            ..one_shot.clone()
        };
        assert!(!fired.due(now + 3_600));

        let repeating = Schedule {
            run_at: None,
            cron_expr: Some("0 9 * * *".into()),
            created_at: "2026-11-14T08:00:00Z".into(),
            ..one_shot.clone()
        };
        assert!(repeating.due(now));
        assert!(!repeating.one_shot());
        // Firing moves the base to now, so the same occurrence never repeats.
        let repeating = Schedule {
            last_triggered_at: Some("2026-11-14T09:00:00Z".into()),
            ..repeating
        };
        assert!(!repeating.due(now));
        assert!(repeating.due(at(2026, 11, 15, 9, 0)));
        assert!(!repeating.due(at(2026, 11, 14, 9, 0)));
        let disabled = Schedule {
            enabled: false,
            ..repeating
        };
        assert!(!disabled.due(at(2026, 11, 15, 9, 0)));
    }

    #[test]
    fn firing_a_row_updates_it_and_disables_only_a_one_shot() {
        let store = store();
        let one_shot = store
            .insert_schedule(
                "ene",
                "reminder",
                "say it",
                Some("2000-01-01T00:00:00Z"),
                None,
            )
            .unwrap();
        let repeating = store
            .insert_schedule("ene", "daily", "say it", None, Some("0 9 * * *"))
            .unwrap();
        let enabled = store.enabled_schedules().unwrap();
        assert_eq!(enabled.len(), 2);
        assert!(
            store
                .mark_schedule_fired(&one_shot.id, "2026-11-14T00:00:00Z", true)
                .unwrap()
        );
        assert!(
            store
                .mark_schedule_fired(&repeating.id, "2026-11-14T00:00:00Z", false)
                .unwrap()
        );
        let (enabled_flag, last): (bool, Option<String>) = store
            .conn()
            .unwrap()
            .query_row(
                "SELECT enabled, last_triggered_at FROM schedules WHERE id = ?1",
                [&one_shot.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(!enabled_flag, "a one-shot row is disabled after firing");
        assert_eq!(last.as_deref(), Some("2026-11-14T00:00:00Z"));
        let enabled = store.enabled_schedules().unwrap();
        assert_eq!(enabled.len(), 1, "the repeating row stays enabled");
        assert_eq!(enabled[0].id, repeating.id);
        assert!(
            !store
                .mark_schedule_fired("missing", "2026-11-14T00:00:00Z", false)
                .unwrap()
        );
        assert_eq!(store.schedules("ene").unwrap().len(), 1);
        assert!(store.cancel_schedule("ene", &repeating.id).unwrap());
        assert!(store.enabled_schedules().unwrap().is_empty());
    }
}
