//! Schedules: stored reminders for the head. Firing them is not wired yet;
//! the storage validates what a tool may register.
use super::{Result, Store, now};
use crate::host::error::HostError;
use rusqlite::params;

/// The inclusive bounds of a five-field cron expression, field by field:
/// minute, hour, day of month, month, day of week (0 and 7 are Sunday).
const CRON_LIMITS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

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
             RETURNING id, title, instruction, run_at, cron_expr, enabled",
            params![id, companion, cron_expr, run_at, title, instruction, now()],
            row_to_schedule,
        )
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
