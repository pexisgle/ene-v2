//! Small parent tools that the design declares before their engines exist:
//! `noop` and the schedule registry, and the schedule firing loop. Every row
//! is resolved to its next firing at creation and after each firing, on the
//! host's wall clock; a due schedule is delivered to the companion's head as
//! a queued message with a short context line, the row records its firing,
//! and a one-shot row is disabled (docs/features/autonomy/schedule.md).
use crate::host::{
    agent::{self, CompanionAgent},
    db::{
        Result, Store,
        schedules::{ScheduleClock, next_occurrence},
    },
    error::HostError,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use jiff::Timestamp;
use rig_core::conversation::Author;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// The firing loop runs at most once per this real-time interval, so a burst
/// of frames (and the frames a client request adds) never fires twice.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct NoopArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateArgs {
    pub(crate) title: String,
    /// What the head should do when the schedule fires.
    pub(crate) instruction: String,
    /// A one-shot time: RFC 3339 with an offset, or a local date and time
    /// read on the host's wall clock.
    #[serde(default)]
    pub(crate) run_at: Option<String>,
    /// A repeating 5-field cron expression.
    #[serde(default)]
    pub(crate) cron_expr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelArgs {
    pub(crate) schedule_id: String,
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<NoopArgs, _>(
        "noop",
        "Do nothing this turn: stay silent and keep watching.",
        ToolOptions::default(),
        run_tool(|_: &mut World, _: Entity, _: &str, _: NoopArgs| {
            Ok(json!({"ok": true}).to_string())
        }),
    )
    .add_open_tool::<CreateArgs, _>(
        "schedule_create",
        "Register a reminder or repeating schedule.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| create(world, agent, args)),
    )
    .add_open_tool::<ListArgs, _>(
        "schedule_list",
        "List your active schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| list(world, agent, args)),
    )
    .add_open_tool::<CancelArgs, _>(
        "schedule_cancel",
        "Remove one of your schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| cancel(world, agent, args)),
    );
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))
}

fn create(world: &mut World, agent: Entity, args: CreateArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let clock = world.resource::<ScheduleClock>().clone();
    let schedule = store.insert_schedule(
        &companion,
        &args.title,
        &args.instruction,
        args.run_at.as_deref(),
        args.cron_expr.as_deref(),
        &clock,
    )?;
    Ok(json!({
        "schedule_id": schedule.id,
        "next_run_at": schedule.next_run_at,
        "time_zone": clock.name(),
    })
    .to_string())
}

fn list(world: &mut World, agent: Entity, _args: ListArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let schedules: Vec<serde_json::Value> = store
        .schedules(&companion)?
        .into_iter()
        .map(|schedule| {
            json!({
                "schedule_id": schedule.id,
                "title": schedule.title,
                "run_at": schedule.run_at,
                "cron_expr": schedule.cron_expr,
                "next_run_at": schedule.next_run_at,
            })
        })
        .collect();
    Ok(json!({"schedules": schedules}).to_string())
}

fn cancel(world: &mut World, agent: Entity, args: CancelArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let removed = store.cancel_schedule(&companion, &args.schedule_id)?;
    Ok(json!({"schedule_id": args.schedule_id, "removed": removed}).to_string())
}

/// Delivers every schedule due at `now` and records the firing. Returns how
/// many fired. The stored `next_run_at` is compared in SQL, so a poll does no
/// per-row parsing; the next time of a repeating row is computed once, after
/// it fires. The firing is recorded before the message is sent, so a storage
/// failure delays a schedule instead of repeating it.
pub(crate) fn poll_schedules(world: &mut World, now: Timestamp) -> usize {
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return 0;
    };
    let Some(clock) = world.get_resource::<ScheduleClock>().cloned() else {
        tracing::warn!("schedules cannot fire without the host's wall clock");
        return 0;
    };
    let schedules = match store.due_schedules(now) {
        Ok(schedules) => schedules,
        Err(error) => {
            tracing::warn!(%error, "schedules could not be read");
            return 0;
        }
    };
    let mut fired = 0;
    for schedule in schedules {
        let Some(agent) = agent::agent_of(world, &schedule.companion_id) else {
            continue;
        };
        // A one-shot ends now; a repeating row moves to its first occurrence
        // after this firing, so missed occurrences are not fired one by one.
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
        match store.mark_schedule_fired(&schedule.id, now, next_run_at) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(%error, schedule = %schedule.id, "a firing schedule was not recorded");
                continue;
            }
        }
        // The head decides what to do; the reason is the only extra context it
        // gets, and the message waits for a running turn instead of cutting in.
        let text = format!(
            "[スケジュール実行: {}] {}",
            schedule.title, schedule.instruction
        );
        // The row records its firing either way; the notice says when the
        // companion's model is not bound and the turn does not start.
        agent::deliver(
            world,
            agent,
            &schedule.companion_id,
            text,
            DeliveryMode::Queue,
            Author::plugin("ene-host"),
        );
        fired += 1;
    }
    fired
}

/// Checks the schedules once a real-time second and delivers the due ones.
fn fire_due_schedules(world: &mut World) {
    let now = world
        .get_resource::<ScheduleClock>()
        .map_or_else(Timestamp::now, ScheduleClock::now);
    let fired = poll_schedules(world, now);
    if fired > 0 {
        tracing::debug!(fired, "schedules fired");
    }
}

/// Registers the small parent tools and the schedule firing loop.
pub(crate) struct AutonomyPlugin;

impl Plugin for AutonomyPlugin {
    fn build(&self, app: &mut App) {
        register(app);
        app.init_resource::<ScheduleClock>().add_systems(
            Update,
            fire_due_schedules
                .after(AgentLoop)
                .run_if(on_real_timer(POLL_INTERVAL).or_else(run_once)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::{Harness, text_turn, texts};
    use jiff::civil::date;

    /// A UTC instant at whole-second precision.
    fn utc(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
        jiff::tz::TimeZone::UTC
            .to_zoned(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
            .timestamp()
    }

    /// The same civil time read in Tokyo.
    fn tokyo(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
        jiff::tz::TimeZone::get("Asia/Tokyo")
            .unwrap()
            .to_zoned(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
            .timestamp()
    }

    fn row(store: &Store, id: &str) -> (bool, Option<String>, Option<String>) {
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

    fn incoming(world: &mut World) -> usize {
        // A `Deliver` triggered from a test is an observer's deferred command.
        world.flush();
        let mut query = world.query_filtered::<Entity, With<Incoming>>();
        query.iter(world).count()
    }

    #[test]
    fn a_due_one_shot_fires_on_a_real_frame_and_is_then_disabled() {
        let mut harness = Harness::new([text_turn("リマインドの時間だよ")]);
        let clock = ScheduleClock::fixed(jiff::tz::TimeZone::UTC, utc(2026, 11, 14, 8, 0));
        let schedule = harness
            .store()
            .insert_schedule(
                &harness.companion,
                "朝会",
                "9時に集合と伝える",
                Some("2000-01-01T00:00:00Z"),
                None,
                &clock,
            )
            .unwrap();
        let agent = harness.companion_agent();
        let before = harness.conversation(agent).messages().len();

        harness
            .app
            .world_mut()
            .insert_resource(bevy_time::TimeUpdateStrategy::ManualDuration(POLL_INTERVAL));
        harness.tick();
        assert_eq!(incoming(harness.app.world_mut()), 1);
        let (enabled, last, next) = row(&harness.store(), &schedule.id);
        assert!(!enabled, "a fired one-shot row is disabled");
        assert!(last.is_some(), "the firing time is recorded");
        assert_eq!(next, None, "a one-shot has no next time");

        harness.run_until(|world| {
            let Some(conversation) = world.get::<Conversation>(agent) else {
                return false;
            };
            conversation.entries().any(|entry| {
                super::agent::entry_text(&entry)
                    .is_some_and(|text| text.contains("リマインドの時間だよ"))
            })
        });
        let texts = texts(harness.conversation(agent));
        assert_eq!(texts.len(), before + 2);
        assert!(
            texts[before].contains("[スケジュール実行: 朝会] "),
            "the context line names the schedule: {:?}",
            texts[before]
        );
        assert!(texts[before].contains("9時に集合と伝える"));
        assert_eq!(texts[before + 1], "リマインドの時間だよ");

        let delivered = harness.conversation(agent).messages().len();
        harness
            .app
            .world_mut()
            .insert_resource(bevy_time::TimeUpdateStrategy::ManualDuration(POLL_INTERVAL));
        harness.tick();
        assert_eq!(harness.conversation(agent).messages().len(), delivered);
    }

    #[test]
    fn a_cron_row_fires_at_nine_on_the_host_wall_clock() {
        let clock = ScheduleClock::fixed(
            jiff::tz::TimeZone::get("Asia/Tokyo").unwrap(),
            tokyo(2026, 11, 14, 8, 0),
        );
        let mut harness = Harness::with_clock([], clock.clone());
        let schedule = harness
            .store()
            .insert_schedule(
                &harness.companion,
                "定時",
                "声をかける",
                None,
                Some("0 9 * * *"),
                &clock,
            )
            .unwrap();
        // Local 09:00 in Tokyo is 00:00 UTC; reading the expression in UTC
        // would have picked 09:00 UTC instead.
        assert_eq!(
            schedule.next_run_at.as_deref(),
            Some("2026-11-14T00:00:00Z")
        );
        assert_eq!(
            poll_schedules(harness.app.world_mut(), tokyo(2026, 11, 14, 8, 59)),
            0
        );
        let now = tokyo(2026, 11, 14, 9, 0);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 1);
        assert_eq!(incoming(harness.app.world_mut()), 1);
        // The same occurrence never fires twice.
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 0);
        assert_eq!(incoming(harness.app.world_mut()), 1);
        assert_eq!(
            poll_schedules(harness.app.world_mut(), utc(2026, 11, 14, 1, 0)),
            0
        );
        // The next occurrence does, and the repeating row stays enabled.
        assert_eq!(
            poll_schedules(harness.app.world_mut(), tokyo(2026, 11, 15, 9, 0)),
            1
        );
        let (enabled, last, next) = row(&harness.store(), &schedule.id);
        assert!(enabled, "a repeating row stays enabled");
        assert_eq!(last.as_deref(), Some("2026-11-15T00:00:00Z"));
        assert_eq!(next.as_deref(), Some("2026-11-16T00:00:00Z"));
    }

    #[test]
    fn a_missed_occurrence_fires_once_after_downtime() {
        let zone = jiff::tz::TimeZone::get("Asia/Tokyo").unwrap();
        let created = ScheduleClock::fixed(zone.clone(), tokyo(2026, 11, 14, 8, 0));
        let now = tokyo(2026, 11, 17, 12, 0);
        let mut harness = Harness::with_clock([], ScheduleClock::fixed(zone, now));
        let schedule = harness
            .store()
            .insert_schedule(
                &harness.companion,
                "定時",
                "声をかける",
                None,
                Some("0 9 * * *"),
                &created,
            )
            .unwrap();
        assert_eq!(
            schedule.next_run_at.as_deref(),
            Some("2026-11-14T00:00:00Z")
        );
        // Three missed mornings fire once, at the wake-up, not once per day.
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 1);
        assert_eq!(incoming(harness.app.world_mut()), 1);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 0);
        assert_eq!(
            poll_schedules(harness.app.world_mut(), tokyo(2026, 11, 17, 23, 0)),
            0
        );
        let (enabled, last, next) = row(&harness.store(), &schedule.id);
        assert!(enabled);
        assert_eq!(last.as_deref(), Some("2026-11-17T03:00:00Z"));
        assert_eq!(next.as_deref(), Some("2026-11-18T00:00:00Z"));
        assert_eq!(incoming(harness.app.world_mut()), 1);
    }

    #[test]
    fn a_schedule_without_an_agent_is_left_for_later() {
        let mut harness = Harness::new([]);
        let clock = ScheduleClock::fixed(jiff::tz::TimeZone::UTC, utc(2026, 11, 14, 8, 0));
        let schedule = harness
            .store()
            .insert_schedule(
                "missing-companion",
                "no head",
                "never delivered",
                Some("2000-01-01T00:00:00Z"),
                None,
                &clock,
            )
            .unwrap();
        assert_eq!(
            poll_schedules(harness.app.world_mut(), utc(2026, 11, 14, 9, 0)),
            0
        );
        let (enabled, last, next) = row(&harness.store(), &schedule.id);
        assert!(enabled, "nothing was fired, so nothing was disabled");
        assert_eq!(last, None);
        assert_eq!(next.as_deref(), Some("2000-01-01T00:00:00Z"));
        assert_eq!(incoming(harness.app.world_mut()), 0);
    }
}
