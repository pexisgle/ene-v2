use super::*;
use crate::util;
use jiff::civil::date;

fn utc(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
    TimeZone::UTC
        .to_zoned_datetime(date(year, month, day).at(hour, minute, 0, 0))
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
fn jiff_stamps_keep_whole_second_utc_text_and_accepted_inputs() {
    let times = [
        "2026-01-01T00:00:00.000000001Z",
        "2026-01-01T00:00:00.999999999Z",
        "2026-01-01T00:00:01Z",
    ]
    .map(|text| text.parse::<Timestamp>().unwrap());
    assert_eq!(
        times.map(stamp),
        [
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        ]
    );
    assert_ne!(times[0].to_string(), stamp(times[0]));
    let mut stored = times.map(stamp);
    stored.sort();
    assert_eq!(stored, times.map(stamp));
    let expected: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
    for text in [
        "2026-01-01T09:00:00+09:00",
        "2026-01-01 00:00:00Z",
        "2026-01-01t00:00:00z",
    ] {
        assert_eq!(parse_run_at(text, &TimeZone::UTC).unwrap(), expected);
        assert_eq!(util::epoch(text), Some(expected.as_second()));
    }
    assert!(!util::now().contains('.'));
}

#[test]
fn a_cron_expression_must_name_exactly_five_fields() {
    let next = |text: &str| next_occurrence(text, Timestamp::UNIX_EPOCH, &TimeZone::UTC);
    for expression in [
        "",
        "* * * *",
        "* * * * * *",
        "* * * * * * *",
        "not a cron",
        "@daily",
        "@yearly",
        " @hourly ",
    ] {
        assert!(next(expression).is_err(), "{expression}");
    }
    assert!(next(&"* ".repeat(200)).is_err());
    assert!(next("* * * * *").is_ok());
    assert!(next("0 9 * * 1-5").is_ok());
    let store = store();
    let repo = store.schedule_repo();
    let clock = utc_clock();
    assert!(matches!(
        repo.insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: None,
                cron_expr: Some("0 9 * *"),
            },
            &clock,
        ),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn a_schedule_takes_exactly_one_valid_time() {
    let store = store();
    let repo = store.schedule_repo();
    let clock = utc_clock();
    assert!(matches!(
        repo.insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: None,
                cron_expr: None,
            },
            &clock,
        ),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        repo.insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: Some("2030-01-01T09:00:00Z"),
                cron_expr: Some("0 9 * * *"),
            },
            &clock,
        ),
        Err(StoreError::Invalid(_))
    ));
    for invalid in ["tomorrow", "2030-02-30T09:00:00", "2030-01-01"] {
        assert!(
            matches!(
                repo.insert_schedule(
                    NewSchedule {
                        companion: "ene",
                        title: "t",
                        instruction: "i",
                        run_at: Some(invalid),
                        cron_expr: None,
                    },
                    &clock,
                ),
                Err(StoreError::Invalid(_))
            ),
            "{invalid}"
        );
    }
    let absolute = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: Some("2030-01-01T09:00:00Z"),
                cron_expr: None,
            },
            &clock,
        )
        .unwrap();
    assert_eq!(
        absolute.next_run_at.as_deref(),
        Some("2030-01-01T09:00:00Z")
    );
    let local = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: Some("2030-01-01T09:00:00"),
                cron_expr: None,
            },
            &clock,
        )
        .unwrap();
    assert_eq!(local.next_run_at.as_deref(), Some("2030-01-01T09:00:00Z"));
    assert!(
        repo.insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: None,
                cron_expr: Some("* * * * *"),
            },
            &clock,
        )
        .is_ok()
    );
}

#[test]
fn a_local_run_at_is_read_in_the_host_time_zone_not_utc() {
    let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
    let clock = ScheduleClock::fixed(tokyo.clone(), utc(2026, 11, 13, 0, 0));
    let store = store();
    let repo = store.schedule_repo();
    let schedule = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: Some("2026-11-15T09:00:00"),
                cron_expr: None,
            },
            &clock,
        )
        .unwrap();
    assert_eq!(
        schedule.next_run_at.as_deref(),
        Some("2026-11-15T00:00:00Z"),
        "local 09:00 is 00:00 UTC in Tokyo"
    );
    let schedule = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "t",
                instruction: "i",
                run_at: Some("2026-11-15T09:00:00+09:00"),
                cron_expr: None,
            },
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
    let repo = store.schedule_repo();
    let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
    let clock = ScheduleClock::fixed(tokyo, utc(2026, 11, 13, 23, 0));
    let one_shot = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "reminder",
                instruction: "say it",
                run_at: Some("2000-01-01T00:00:00Z"),
                cron_expr: None,
            },
            &clock,
        )
        .unwrap();
    let repeating = repo
        .insert_schedule(
            NewSchedule {
                companion: "ene",
                title: "daily",
                instruction: "say it",
                run_at: None,
                cron_expr: Some("0 9 * * *"),
            },
            &clock,
        )
        .unwrap();
    assert_eq!(
        repeating.next_run_at.as_deref(),
        Some("2026-11-14T00:00:00Z")
    );
    let due = repo.due_schedules(clock.now()).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, one_shot.id);
    assert_eq!(repo.schedules("ene").unwrap().len(), 2);

    assert!(
        repo.mark_schedule_fired(&one_shot.id, clock.now(), None)
            .unwrap()
    );
    assert!(
        !repo
            .mark_schedule_fired("missing", clock.now(), None)
            .unwrap()
    );
    let (enabled, last, next) = stored(&store, &one_shot.id);
    assert!(!enabled, "a one-shot row is disabled after firing");
    assert_eq!(last.as_deref(), Some("2026-11-13T23:00:00Z"));
    assert_eq!(next, None);

    let fired_at = utc(2026, 11, 14, 0, 0);
    let due = repo.due_schedules(fired_at).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, repeating.id);
    assert!(
        repo.mark_schedule_fired(&repeating.id, fired_at, Some(utc(2026, 11, 15, 0, 0)))
            .unwrap()
    );
    let (enabled, last, next) = stored(&store, &repeating.id);
    assert!(enabled, "a repeating row stays enabled");
    assert_eq!(last.as_deref(), Some("2026-11-14T00:00:00Z"));
    assert_eq!(next.as_deref(), Some("2026-11-15T00:00:00Z"));
    assert_eq!(repo.due_schedules(fired_at).unwrap().len(), 0);
    assert_eq!(
        repo.due_schedules(utc(2026, 11, 15, 0, 0)).unwrap().len(),
        1
    );

    assert!(repo.cancel_schedule("ene", &repeating.id).unwrap());
    assert_eq!(
        repo.due_schedules(utc(2026, 11, 16, 0, 0)).unwrap().len(),
        0
    );
}

fn stored(store: &Store, id: &str) -> (bool, Option<String>, Option<String>) {
    store
        .conn()
        .query_row(
            "SELECT enabled, last_triggered_at, next_run_at FROM schedules WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}
