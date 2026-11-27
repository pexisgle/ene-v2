//! Small parent tools that the design declares before their engines exist:
//! `noop` and the schedule registry, and the schedule firing loop. Every row
//! is resolved to its next firing at creation and after each firing, on the
//! host's wall clock; a due schedule is delivered to the companion's head as
//! a queued message with a short context line, the row records its firing,
//! and a one-shot row is disabled (docs/features/tasks.md).
use crate::host::{
    agent::{self, CompanionAgent},
    db::{Result, ScheduleClock, Store},
    error::HostError,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use jiff::Timestamp;
use rig_core::conversation::{Author, AuthorKind, RequestId};
use rig_ecs::prelude::*;
use rig_ecs::{Persist, WriteLogs, session_log::SessionLog};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

/// The firing loop runs at most once per this real-time interval, so a burst
/// of frames (and the frames a client request adds) never fires twice.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const SCHEDULE_AUTHOR: &str = "ene-host";

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

fn is_eyes_silent_turn(turn: Option<&Turn>, conversation: Option<&Conversation>) -> bool {
    let (Some(turn), Some(conversation)) = (turn, conversation) else {
        return false;
    };
    !conversation
        .entries()
        .skip(conversation.start_of(turn.start))
        .any(|entry| entry.is_users_own())
        && conversation
            .asked(turn.start)
            .filter(rig_core::conversation::EntryRef::is_request)
            .last()
            .is_some_and(|entry| entry.author.is_some_and(super::eyes::is_eyes))
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<NoopArgs, _>(
        "noop",
        "Do nothing this turn: stay silent and keep watching.",
        ToolOptions::default(),
        run_tool(|world, entity, _, _: NoopArgs| {
            let turn = world
                .get::<ActiveTurn>(entity)
                .and_then(|active| world.get::<Turn>(active.turn()));
            if is_eyes_silent_turn(turn, world.get::<Conversation>(entity)) {
                world.trigger(StopTurn::plugin_alone(
                    entity,
                    "ene-host",
                    super::eyes::WATCHED,
                ));
            }
            Ok(json!({"ok": true}).to_string())
        }),
    )
    .add_open_tool::<CreateArgs, _>(
        "schedule_create",
        "Register a reminder or repeating schedule.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args: CreateArgs| {
            let companion = companion_id(world.get::<CompanionAgent>(agent))?;
            create_schedule(
                world.resource::<Store>(),
                world.resource::<ScheduleClock>(),
                companion,
                &args,
            )
        }),
    )
    .add_open_tool::<ListArgs, _>(
        "schedule_list",
        "List your active schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, _args: ListArgs| {
            let companion = companion_id(world.get::<CompanionAgent>(agent))?;
            list_schedules(world.resource::<Store>(), companion)
        }),
    )
    .add_open_tool::<CancelArgs, _>(
        "schedule_cancel",
        "Remove one of your schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args: CancelArgs| {
            let companion = companion_id(world.get::<CompanionAgent>(agent))?;
            cancel_schedule(world.resource::<Store>(), companion, &args.schedule_id)
        }),
    );
}

fn companion_id(agent: Option<&CompanionAgent>) -> Result<&str> {
    agent
        .map(|agent| agent.companion_id.as_str())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))
}

fn create_schedule(
    store: &Store,
    clock: &ScheduleClock,
    companion: &str,
    args: &CreateArgs,
) -> Result<String> {
    let schedule = store.schedule_repo().insert_schedule(
        super::db::schedules::NewSchedule {
            companion,
            title: &args.title,
            instruction: &args.instruction,
            run_at: args.run_at.as_deref(),
            cron_expr: args.cron_expr.as_deref(),
        },
        clock,
    )?;
    Ok(json!({
        "schedule_id": schedule.id,
        "next_run_at": schedule.next_run_at,
        "time_zone": clock.name(),
    })
    .to_string())
}

fn list_schedules(store: &Store, companion: &str) -> Result<String> {
    let schedules: Vec<serde_json::Value> = store
        .schedule_repo()
        .schedules(companion)?
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

fn cancel_schedule(store: &Store, companion: &str, schedule_id: &str) -> Result<String> {
    let removed = store
        .schedule_repo()
        .cancel_schedule(companion, schedule_id)?;
    Ok(json!({"schedule_id": schedule_id, "removed": removed}).to_string())
}

fn schedule_author(occurrence_key: &str) -> Author {
    let mut author = Author::plugin(SCHEDULE_AUTHOR);
    author.request = Some(RequestId(occurrence_key.to_owned()));
    author
}

fn scheduled_request_key(author: &Author) -> Option<&str> {
    match &author.kind {
        AuthorKind::Plugin(name) if name == SCHEDULE_AUTHOR => {
            author.request.as_ref().map(|request| request.0.as_str())
        }
        _ => None,
    }
}

fn conversation_has_occurrence(conversation: Option<&Conversation>, occurrence_key: &str) -> bool {
    conversation.is_some_and(|conv| {
        conv.entries()
            .any(|entry| entry.author.and_then(scheduled_request_key) == Some(occurrence_key))
    })
}

/// Confirms occurrences after Rig has written the companion's conversation log.
fn confirm_delivered_schedules(
    log: Res<SessionLog>,
    store: Res<Store>,
    clock: Res<ScheduleClock>,
    agents: Query<(&CompanionAgent, &Conversation)>,
    mut commands: Commands,
) {
    if log.into_inner().failure().is_some() {
        return;
    }
    let schedules = store.into_inner().schedule_repo();
    let now = clock.into_inner().now();
    let result = (|| -> Result<()> {
        for occurrence in schedules.pending_schedule_occurrences()? {
            let conversation = agents.iter().find_map(|(agent, conversation)| {
                (agent.companion_id == occurrence.companion_id && conversation.unsaved().is_empty())
                    .then_some(conversation)
            });
            if conversation_has_occurrence(conversation, &occurrence.occurrence_key) {
                schedules.mark_occurrence_delivered(&occurrence.occurrence_key, now)?;
            }
        }
        Ok(())
    })();
    match result {
        Err(error) if error.is_storage() => {
            commands.run_system_cached_with(super::stop_on_failure, error.to_string());
        }
        Err(error) => tracing::warn!(%error, "pending schedule occurrences could not be read"),
        Ok(()) => {}
    }
}

/// Materializes due schedules into idempotent occurrences and delivers pending
/// ones to their companion's head. Returns how many new messages were queued.
///
/// An occurrence stays `pending` until confirmed in the companion's
/// `Conversation` (matched by `Author.request = Some(RequestId(occurrence_key))`):
/// - Transient delivery failures (such as an unbound model) keep the occurrence
///   pending and retry cleanly once the model is bound.
/// - Restarts after the conversation persisted the message mark the occurrence
///   `delivered` without queuing a duplicate.
enum OccurrenceOutcome {
    Queued,
    Skipped,
    FatalStorage,
}

pub(crate) fn poll_schedules(world: &mut World, now: Timestamp) -> usize {
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return 0;
    };
    let Some(clock) = world.get_resource::<ScheduleClock>().cloned() else {
        tracing::warn!("schedules cannot fire without the host's wall clock");
        return 0;
    };
    let ready_companions: HashSet<String> = {
        let mut query = world.query::<&CompanionAgent>();
        query
            .iter(world)
            .map(|agent| agent.companion_id.clone())
            .collect()
    };
    let occurrences =
        match store
            .schedule_repo()
            .materialize_due_occurrences(now, &clock, &ready_companions)
        {
            Ok(occurrences) => occurrences,
            Err(error) => {
                if error.is_storage() {
                    super::fail(world, &error.to_string());
                } else {
                    tracing::warn!(%error, "schedules could not be materialized");
                }
                return 0;
            }
        };
    let mut queued_inbox: HashSet<(Entity, String)> = {
        let mut query = world.query::<(&ForAgent, &Incoming)>();
        query
            .iter(world)
            .filter_map(|(of, incoming)| {
                scheduled_request_key(&incoming.author).map(|key| (of.0, key.to_owned()))
            })
            .collect()
    };
    let mut fired = 0;
    for occ in occurrences {
        match dispatch_due_occurrence(world, &store, &mut queued_inbox, &occ) {
            OccurrenceOutcome::Queued => fired += 1,
            OccurrenceOutcome::Skipped => {}
            OccurrenceOutcome::FatalStorage => return fired,
        }
    }
    fired
}

fn dispatch_due_occurrence(
    world: &mut World,
    store: &Store,
    queued_inbox: &mut HashSet<(Entity, String)>,
    occ: &super::db::schedules::ScheduleOccurrence,
) -> OccurrenceOutcome {
    let Some(agent) = agent::agent_of(world, &occ.companion_id) else {
        return OccurrenceOutcome::Skipped;
    };
    if queued_inbox.contains(&(agent, occ.occurrence_key.clone()))
        || conversation_has_occurrence(world.get::<Conversation>(agent), &occ.occurrence_key)
    {
        return OccurrenceOutcome::Skipped;
    }
    // The head decides what to do; the reason is the only extra context it
    // gets, and the message waits for a running turn instead of cutting in.
    let text = format!("[スケジュール実行: {}] {}", occ.title, occ.instruction);
    if agent::deliver(
        world,
        agent,
        &occ.companion_id,
        text,
        DeliveryMode::Queue,
        schedule_author(&occ.occurrence_key),
    ) {
        // Flush the deferred `Deliver` spawn so a back-to-back poll in the
        // same frame sees the queued `Incoming` entity and stays idempotent.
        world.flush();
        queued_inbox.insert((agent, occ.occurrence_key.clone()));
        return OccurrenceOutcome::Queued;
    }
    if let Err(error) = store
        .schedule_repo()
        .record_occurrence_attempt(&occ.occurrence_key, "model unavailable")
        && error.is_storage()
    {
        super::fail(world, &error.to_string());
        return OccurrenceOutcome::FatalStorage;
    }
    OccurrenceOutcome::Skipped
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
        app.init_resource::<ScheduleClock>()
            .add_systems(
                Update,
                fire_due_schedules
                    .run_if(on_real_timer(POLL_INTERVAL).or_else(run_once))
                    .after(AgentLoop),
            )
            .add_systems(
                Persist,
                confirm_delivered_schedules
                    .after(WriteLogs)
                    .run_if(resource_exists::<SessionRestored>),
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
            .to_zoned_datetime(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
            .timestamp()
    }

    /// The same civil time read in Tokyo.
    fn tokyo(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
        jiff::tz::TimeZone::get("Asia/Tokyo")
            .unwrap()
            .to_zoned_datetime(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
            .timestamp()
    }

    fn row(store: &Store, id: &str) -> (bool, Option<String>, Option<String>) {
        store
            .conn()
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
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: &harness.companion,
                    title: "朝会",
                    instruction: "9時に集合と伝える",
                    run_at: Some("2000-01-01T00:00:00Z"),
                    cron_expr: None,
                },
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
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: &harness.companion,
                    title: "定時",
                    instruction: "声をかける",
                    run_at: None,
                    cron_expr: Some("0 9 * * *"),
                },
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
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: &harness.companion,
                    title: "定時",
                    instruction: "声をかける",
                    run_at: None,
                    cron_expr: Some("0 9 * * *"),
                },
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
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: "missing-companion",
                    title: "no head",
                    instruction: "never delivered",
                    run_at: Some("2000-01-01T00:00:00Z"),
                    cron_expr: None,
                },
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

    #[test]
    fn b10_one_shot_survives_unbound_model_and_delivers_once_when_bound() {
        let clock = ScheduleClock::fixed(jiff::tz::TimeZone::UTC, utc(2026, 11, 14, 8, 0));
        let mut harness = Harness::with_clock([], clock.clone());
        // Make model binding fail so delivery cannot start a turn yet.
        harness
            .app
            .world_mut()
            .insert_resource(crate::host::model::ModelFactory::default());
        let schedule = harness
            .store()
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: &harness.companion,
                    title: "大事な予定",
                    instruction: "会議が始まるよ",
                    run_at: Some("2026-11-14T08:00:00Z"),
                    cron_expr: None,
                },
                &clock,
            )
            .unwrap();
        let now = utc(2026, 11, 14, 8, 5);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 0);
        let pending = harness
            .store()
            .schedule_repo()
            .pending_schedule_occurrences()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].schedule_id, schedule.id);
        assert_eq!(pending[0].attempts, 1);
        assert_eq!(incoming(harness.app.world_mut()), 0);

        // Restart with a working model bound: the pending occurrence must deliver once.
        harness.restart([text_turn("会議の時間だね")]);
        harness.app.world_mut().insert_resource(clock);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 1);
        // Immediate re-poll stays idempotent while in inbox.
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 0);

        let agent = harness.companion_agent();
        harness.run_until(|world| {
            let Some(conv) = world.get::<Conversation>(agent) else {
                return false;
            };
            conv.entries().any(|entry| {
                super::agent::entry_text(&entry).is_some_and(|t| t.contains("会議の時間だね"))
            })
        });
        assert!(
            harness
                .store()
                .schedule_repo()
                .pending_schedule_occurrences()
                .unwrap()
                .is_empty(),
            "confirmed occurrence must transition to delivered"
        );
        // Simulate a crash right before SQLite ACK: resetting status back to pending
        // and restarting must NOT duplicate the occurrence in Conversation.
        let key = pending[0].occurrence_key.clone();
        harness
            .store()
            .conn()
            .execute(
                "UPDATE schedule_occurrences SET status = 'pending', delivered_at = NULL \
                 WHERE occurrence_key = ?1",
                [&key],
            )
            .unwrap();
        harness.restart([]);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 0);
        harness.tick();
        assert!(
            harness
                .store()
                .schedule_repo()
                .pending_schedule_occurrences()
                .unwrap()
                .is_empty(),
            "restart must reconcile delivered occurrence from Conversation without duplicating"
        );
        let agent = harness.companion_agent();
        let matches = harness
            .conversation(agent)
            .entries()
            .filter(|entry| {
                entry
                    .author
                    .and_then(|author| author.request.as_ref())
                    .is_some_and(|req| req.0 == key)
            })
            .count();
        assert_eq!(matches, 1);
    }

    #[test]
    fn a_failed_conversation_append_leaves_the_one_shot_pending() {
        let now = utc(2026, 11, 14, 8, 0);
        let clock = ScheduleClock::fixed(jiff::tz::TimeZone::UTC, now);
        let mut harness = Harness::with_clock([text_turn("予定の時刻です")], clock.clone());
        let schedule = harness
            .store()
            .schedule_repo()
            .insert_schedule(
                crate::host::db::schedules::NewSchedule {
                    companion: &harness.companion,
                    title: "保存の確認",
                    instruction: "予定を伝える",
                    run_at: Some("2026-11-14T08:00:00Z"),
                    cron_expr: None,
                },
                &clock,
            )
            .unwrap();
        let agent_id = agent::companion_agent_id(&harness.companion);
        let logs = harness.root.path().join("rig/logs");
        let log = logs.join(format!("{agent_id}.jsonl"));
        let saved = logs.join("original-log-backup");
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 1);
        let key = harness
            .store()
            .schedule_repo()
            .pending_schedule_occurrences()
            .unwrap()[0]
            .occurrence_key
            .clone();
        let agent = harness.companion_agent();
        let (blocked, backup) = (log.clone(), saved.clone());
        harness.app.add_systems(
            Persist,
            (move |agents: Query<&Conversation>| {
                if !conversation_has_occurrence(agents.get(agent).ok(), &key) || blocked.is_dir() {
                    return;
                }
                if blocked.exists() {
                    std::fs::rename(&blocked, &backup).unwrap();
                }
                std::fs::create_dir_all(&blocked).unwrap();
            })
            .before(WriteLogs),
        );
        harness.run_until(|world| world.resource::<SessionLog>().failure().is_some());

        let pending = harness
            .store()
            .schedule_repo()
            .pending_schedule_occurrences()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].schedule_id, schedule.id);

        std::fs::remove_dir(&log).unwrap();
        if saved.exists() {
            std::fs::rename(&saved, &log).unwrap();
        }
        harness.restart([text_turn("予定の時刻です")]);
        assert_eq!(poll_schedules(harness.app.world_mut(), now), 1);
        harness.run_until(|world| {
            world
                .resource::<Store>()
                .schedule_repo()
                .pending_schedule_occurrences()
                .unwrap()
                .is_empty()
        });
    }
}
