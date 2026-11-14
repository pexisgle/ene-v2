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
use std::time::{Duration, Instant};

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

/// The last real-time poll, so the loop checks due schedules once a second
/// however often the app runs a frame.
#[derive(Resource, Default)]
pub(crate) struct SchedulePoll {
    last: Option<Instant>,
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
        .ok_or_else(|| HostError::internal("this tool belongs to a companion"))
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
        world.trigger(
            Deliver::new(agent, text, DeliveryMode::Queue).with_author(Author::plugin("ene-host")),
        );
        fired += 1;
    }
    fired
}

/// Checks the schedules once a real-time second and delivers the due ones.
fn fire_due_schedules(world: &mut World) {
    {
        let Some(mut poll) = world.get_resource_mut::<SchedulePoll>() else {
            return;
        };
        let now = Instant::now();
        if poll
            .last
            .is_some_and(|last| now.duration_since(last) < POLL_INTERVAL)
        {
            return;
        }
        poll.last = Some(now);
    }
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
            .init_resource::<SchedulePoll>()
            .add_systems(Update, fire_due_schedules.after(AgentLoop));
    }
}

/// A headless app with one companion, the kernel, the session log and the
/// autonomy plugins, for the tests of this module and `staged`.
#[cfg(test)]
pub(crate) mod testing {
    use crate::host::{
        self,
        agent::{self, TestModel},
        db::{Store, schedules::ScheduleClock},
        mind, model,
        notify::Notifier,
        permission,
        resources::{DeviceHub, DeviceTimeout, HostState, Perception, Waiting},
    };
    use bevy_ecs::prelude::*;
    use ene_rpc as w;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::{ErasedHandler, adapters::ModelAdapter};
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use rig_ecs::prelude::*;
    use rig_ecs::session_log::SessionStore;
    use rig_memory::DirStore;
    use std::{
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };

    pub(crate) struct Harness {
        pub(crate) app: App,
        pub(crate) companion: String,
        pub(crate) root: tempfile::TempDir,
        wakes: mpsc::Receiver<()>,
    }

    impl Harness {
        pub(crate) fn new(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) -> Self {
            Self::with_clock(turns, ScheduleClock::system())
        }

        /// A harness whose schedules are read on `clock`'s wall clock.
        pub(crate) fn with_clock(
            turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
            clock: ScheduleClock,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            let model = MockCompletionModel::from_stream_turns(turns);
            let state = root.path();
            let store = Store::open(&state.join("host.sqlite3")).unwrap();
            let logs = DirStore::open(state.join("rig")).unwrap();
            let (sender, wakes) = mpsc::channel();
            let mut app = App::new();
            app.set_error_handler(bevy_ecs::error::warn)
                .add_plugins(TaskPoolPlugin::default())
                .insert_resource(store)
                .insert_resource(clock)
                .insert_resource(model::Credentials::default())
                .insert_resource(DeviceHub::default())
                .insert_resource(HostState::default())
                .insert_resource(Notifier::new())
                .insert_resource(Waiting::default())
                .insert_resource(Perception::default())
                .insert_resource(permission::Approvals::default())
                .insert_resource(DeviceTimeout(Duration::from_millis(100)))
                .insert_resource(rig_ecs::tasks::Wake::new(move || {
                    let _ = sender.send(());
                }))
                .insert_resource(SessionStore::new(logs))
                .insert_resource(TestModel(Arc::new(move || bound(&model))))
                .init_resource::<crate::host::compaction::InactivityTracker>()
                .add_plugins((
                    AgentPlugin,
                    SessionLogPlugin,
                    agent::AgentPlugin,
                    super::AutonomyPlugin,
                    crate::host::staged::StagedPlugin,
                ));
            app.finish();
            if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
                time.set_max_delta(Duration::from_secs(60));
            }
            app.update();
            let companion = mind::create(
                app.world_mut(),
                w::CreateCompanionRequest {
                    name: "ene".into(),
                    mind: "kind".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .id;
            app.update();
            Self {
                app,
                companion,
                root,
                wakes,
            }
        }

        pub(crate) fn store(&self) -> Store {
            self.app.world().resource::<Store>().clone()
        }

        pub(crate) fn companion_agent(&mut self) -> Entity {
            agent::agent_of(self.app.world_mut(), &self.companion).expect("the companion agent")
        }

        pub(crate) fn conversation(&self, agent: Entity) -> &Conversation {
            self.app
                .world()
                .get::<Conversation>(agent)
                .expect("an agent conversation")
        }

        /// The names under the DirStore's `logs/`, sorted; the session log's
        /// files.
        pub(crate) fn log_names(&self) -> Vec<String> {
            let dir = self.root.path().join("rig").join("logs");
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }

        /// Advances one frame and flushes its notifications.
        pub(crate) fn tick(&mut self) {
            host::tick(&mut self.app);
        }

        pub(crate) fn run_until(&mut self, done: impl Fn(&mut World) -> bool) {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(10) {
                self.tick();
                if done(self.app.world_mut()) {
                    return;
                }
                let _ = self.wakes.recv_timeout(Duration::from_millis(20));
            }
            panic!("the condition did not hold in time");
        }
    }

    pub(crate) fn text_turn(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }

    pub(crate) fn bound(model: &MockCompletionModel) -> model::Bound {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .unwrap()
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            model.clone(),
        ));
        model::Bound {
            connection: rig_ecs::model::Connection {
                spec: Arc::clone(&spec),
                handler: Handler(handler),
            },
        }
    }

    /// The text of every conversation entry, oldest first.
    pub(crate) fn texts(conversation: &Conversation) -> Vec<String> {
        conversation
            .entries()
            .filter_map(|entry| agent::entry_text(&entry))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Harness, text_turn};
    use super::*;
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

        // One real frame runs the firing loop; the message is queued for the
        // head and the row records it.
        harness.app.world_mut().resource_mut::<SchedulePoll>().last = None;
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
        let texts = super::testing::texts(harness.conversation(agent));
        assert_eq!(texts.len(), before + 2);
        assert!(
            texts[before].contains("[スケジュール実行: 朝会] "),
            "the context line names the schedule: {:?}",
            texts[before]
        );
        assert!(texts[before].contains("9時に集合と伝える"));
        assert_eq!(texts[before + 1], "リマインドの時間だよ");

        // The disabled row never fires again, even when the loop polls again.
        let delivered = harness.conversation(agent).messages().len();
        harness.app.world_mut().resource_mut::<SchedulePoll>().last = None;
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
