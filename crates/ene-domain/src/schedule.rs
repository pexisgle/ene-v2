//! Validation and wall-clock calculation for reminders.

use crate::DomainError;
use jiff::{Timestamp, tz::TimeZone};

/// The timing selected for a new reminder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleTiming<'a> {
    /// A single RFC 3339 instant or local date and time.
    Once(&'a str),
    /// A recurring five-field cron expression.
    Recurring(&'a str),
}

impl<'a> ScheduleTiming<'a> {
    /// Selects exactly one timing field, treating blank fields as absent.
    pub fn from_fields(
        run_at: Option<&'a str>,
        cron_expr: Option<&'a str>,
    ) -> Result<Self, DomainError> {
        match (
            run_at.filter(|value| !value.trim().is_empty()),
            cron_expr.filter(|value| !value.trim().is_empty()),
        ) {
            (Some(time), None) => Ok(Self::Once(time)),
            (None, Some(expression)) => Ok(Self::Recurring(expression)),
            (Some(_), Some(_)) => Err(DomainError::Invalid(
                "run_at and cron_expr are mutually exclusive".into(),
            )),
            (None, None) => Err(DomainError::Invalid(
                "a schedule needs exactly one of run_at or cron_expr".into(),
            )),
        }
    }
}

/// Finds the first matching time strictly after `after` in `zone`.
pub fn next_occurrence(
    expression: &str,
    after: Timestamp,
    zone: &TimeZone,
) -> Result<Timestamp, DomainError> {
    if expression.chars().count() > 256 {
        return Err(DomainError::Invalid("cron_expr is too long".into()));
    }
    if expression.contains('@') {
        return Err(DomainError::Invalid(
            "cron_expr must use 5 literal fields, not a nickname".into(),
        ));
    }
    let cron = croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Disallowed)
        .year(croner::parser::Year::Disallowed)
        .build()
        .parse(expression)
        .map_err(|error| DomainError::Invalid(format!("cron_expr is invalid: {error}")))?;
    cron.find_next_occurrence(&after.to_zoned_datetime(zone.clone()), false)
        .map(|at| at.timestamp())
        .map_err(|_| {
            DomainError::Invalid("cron_expr names no time within the search window".into())
        })
}

/// Resolves an offset timestamp or a local date and time in `zone`.
pub fn parse_run_at(text: &str, zone: &TimeZone) -> Result<Timestamp, DomainError> {
    let text = text.trim();
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    let local: jiff::civil::DateTime = text.parse().map_err(|_| {
        DomainError::Invalid("run_at must be an RFC 3339 time or a local date and time".into())
    })?;
    if !text.contains(['T', 't', ' ']) {
        return Err(DomainError::Invalid(
            "run_at must include a time of day".into(),
        ));
    }
    zone.to_zoned_datetime(local)
        .map(|at| at.timestamp())
        .map_err(|_| DomainError::Invalid("run_at is outside the supported range".into()))
}
