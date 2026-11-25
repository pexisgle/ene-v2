//! Evaluate a bounded stream of observations and wake only the active head.
//!
//! [`Session`] is the whole per-companion state; the work split over three
//! files, each owning one group of its fields. [`events`] records what a client
//! reports, [`assess`] asks the perception model what it means, and [`delivery`]
//! hands the resulting reaction to the head. This file keeps the state itself,
//! the assessment-to-reaction rules in [`Session::queue_assessment`], and the
//! [`poll`] loop that drives all three.
use super::{
    agent, background,
    db::Store,
    error::{HostError, Result},
    resources::DeviceHub,
    util::truncate,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Author, AuthorKind, RequestId};
use rig_ecs::prelude::*;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};

const EVALUATION_INTERVAL: Duration = Duration::from_secs(20);
const REACTION_INTERVAL: Duration = Duration::from_secs(60);
const QUIET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const OBSERVATION_LIFETIME: Duration = Duration::from_secs(60);
const PREVIEW_LIFETIME: Duration = Duration::from_secs(180);
const PREVIEW_FRAMES: usize = 8;
const PENDING_IMPORTANT_LIMIT: usize = 4;
const SUMMARY_LIMIT: usize = 300;
const AUTHOR: &str = "ene-eyes";
pub(crate) const WATCHED: &str = "the head chose to watch quietly";

mod assess;
mod delivery;
mod events;

use assess::{Assessment, Evaluation, ImportantObservation, evaluate};
use delivery::{deliver, diagnostic, publish_status, with_notifier};
pub(crate) use events::report_with;

/// The assessment parser and its model plumbing are `assess`'s own; the tests
/// below still drive [`Session::queue_assessment`] with parsed assessments and
/// inspect the request the perception model receives.
#[cfg(test)]
use {
    super::{mind::Role, model::ModelFactory, notify::Notifier, resources::HostRuntime},
    assess::parse_assessment,
    rig_core::message::{Message, UserContent},
};

#[derive(Component)]
pub(crate) struct ReportOfTurn {
    pub(crate) id: RequestId,
}

#[derive(Default, Resource)]
pub(crate) struct Eyes {
    sessions: HashMap<String, Session>,
}

struct Session {
    client: String,
    subscription: String,
    seen: Instant,
    next_evaluation: Instant,
    dirty: bool,
    events: VecDeque<(Instant, Value)>,
    window: Option<Value>,
    activity: Option<(bool, u64)>,
    screen: Option<ScreenSample>,
    recent_screens: VecDeque<ScreenSample>,
    last_evaluated: Option<ScreenSample>,
    previous: String,
    task: Option<background::AbortOnDropHandle<Evaluation>>,
    status: &'static str,
    pending: Option<Reaction>,
    pending_important: VecDeque<Reaction>,
    delivered: Option<Delivered>,
    last_delivery: Instant,
    last_automatic: Option<(Instant, String)>,
    last_important: Option<String>,
}

#[derive(Clone)]
struct ScreenSample {
    id: String,
    at: Instant,
    jpeg: Arc<[u8]>,
}

struct Reaction {
    summary: String,
    kind: ReportKind,
    at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportKind {
    Automatic,
    Heartbeat,
    Important,
    ResponseCheck,
    Watch,
}

impl ReportKind {
    fn important(self) -> bool {
        matches!(self, Self::Important | Self::ResponseCheck)
    }
}

pub(crate) struct Delivered {
    pub(crate) id: RequestId,
    pub(crate) summary: String,
    pub(crate) kind: ReportKind,
}

impl Session {
    fn new(client: String, subscription: String, now: Instant) -> Self {
        Self {
            client,
            subscription,
            seen: now,
            next_evaluation: now + Duration::from_secs(2),
            dirty: false,
            events: VecDeque::new(),
            window: None,
            activity: None,
            screen: None,
            recent_screens: VecDeque::with_capacity(PREVIEW_FRAMES),
            last_evaluated: None,
            previous: String::new(),
            task: None,
            status: "",
            pending: None,
            pending_important: VecDeque::new(),
            delivered: None,
            last_delivery: now,
            last_automatic: None,
            last_important: None,
        }
    }

    fn quiet_summary(&self) -> String {
        if let Some((false, idle_secs)) = self.activity {
            return format!("特に目立った変化はありません（ユーザーは離席中・約{idle_secs}秒）");
        }
        if let Some(window) = self.window.as_ref() {
            let label = window["title"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    window["app"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                });
            if let Some(label) = label {
                return format!(
                    "特に目立った変化はありません（前面: {}）",
                    truncate(label, 120)
                );
            }
        }
        "特に目立った変化はありません（静かに見守り中）".into()
    }

    fn event(&mut self, now: Instant, value: Value) {
        if self.events.len() == 8 {
            self.events.pop_front();
        }
        self.events.push_back((now, value));
    }

    fn valid(&self, hub: &DeviceHub, now: Instant) -> bool {
        now.duration_since(self.seen) <= OBSERVATION_LIFETIME
            && hub.subscription(&self.client).as_deref() == Some(self.subscription.as_str())
    }

    fn queue_assessment(
        &mut self,
        now: Instant,
        assessment: Assessment,
        image: bool,
    ) -> &'static str {
        self.previous = assessment.summary;
        let important = match assessment.important {
            Some(ImportantObservation::ResponseCheck) if image => Some((
                ReportKind::ResponseCheck,
                "画面に、見えているかの確認として「応答して」と表示されている。".into(),
            )),
            Some(ImportantObservation::Finding { summary }) => {
                Some((ReportKind::Important, summary))
            }
            _ => None,
        };
        if let Some((kind, summary)) = important {
            self.previous = summary.clone();
            if self.last_important.as_deref() == Some(summary.as_str()) {
                return "important_duplicate";
            }
            if let Some(waiting) = self
                .pending_important
                .iter_mut()
                .find(|waiting| waiting.summary == summary)
            {
                waiting.at = now;
                return "important_duplicate";
            }
            if self.pending_important.len() == PENDING_IMPORTANT_LIMIT {
                self.pending_important.pop_front();
            }
            self.pending_important.push_back(Reaction {
                summary,
                kind,
                at: now,
            });
            return "important_queued";
        }
        if image {
            self.last_important = None;
        }
        let summary = &self.previous;
        if !assessment.notify {
            if self.pending.is_none()
                && now.duration_since(self.last_delivery) >= QUIET_HEARTBEAT_INTERVAL
            {
                let heartbeat_summary = if summary.trim().is_empty() {
                    self.quiet_summary()
                } else {
                    format!("特に大きな変化はありません（現在の様子: {summary}）")
                };
                self.pending = Some(Reaction {
                    summary: heartbeat_summary,
                    kind: ReportKind::Heartbeat,
                    at: now,
                });
                return "heartbeat_queued";
            }
            return "not_important";
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|event| event.kind == ReportKind::Watch)
        {
            return "watch_priority";
        }
        if let Some((at, last)) = &self.last_automatic {
            if last.eq_ignore_ascii_case(summary) {
                return "duplicate";
            }
            if now.duration_since(*at) < REACTION_INTERVAL {
                return "cooldown";
            }
        }
        self.pending = Some(Reaction {
            summary: summary.clone(),
            kind: ReportKind::Automatic,
            at: now,
        });
        "queued"
    }
}

pub(crate) fn clear(eyes: &mut Eyes, companion: &str) {
    eyes.sessions.remove(companion);
}

fn valid_jpeg(jpeg: &[u8]) -> bool {
    if jpeg.len() > w::MAX_PERCEPTION_JPEG_BYTES {
        return false;
    }
    image::ImageReader::with_format(Cursor::new(jpeg), image::ImageFormat::Jpeg)
        .into_dimensions()
        .is_ok_and(|(width, height)| {
            width > 0 && height > 0 && width.max(height) <= w::MAX_PERCEPTION_FRAME_EDGE
        })
}

pub(crate) fn poll(world: &mut World) {
    if world.resource::<Eyes>().sessions.is_empty() {
        return;
    }
    let hub = world.resource::<DeviceHub>().clone();
    let store = world.resource::<Store>().clone();
    let now = Instant::now();
    world.resource_scope(|world, mut eyes: Mut<Eyes>| {
        eyes.sessions.retain(|companion_id, session| {
            if !session.valid(&hub, now)
                || store
                    .client_repo()
                    .activation_for_client(&session.client)
                    .ok()
                    .flatten()
                    .as_ref()
                    != Some(companion_id)
            {
                return false;
            }
            session
                .recent_screens
                .retain(|sample| now.duration_since(sample.at) <= PREVIEW_LIFETIME);
            if session
                .last_evaluated
                .as_ref()
                .is_some_and(|sample| now.duration_since(sample.at) > PREVIEW_LIFETIME)
            {
                session.last_evaluated = None;
            }
            if session
                .task
                .as_ref()
                .is_some_and(tokio_util::task::AbortOnDropHandle::is_finished)
                && let Some(result) = session.task.take().and_then(background::take)
            {
                publish_status(world, companion_id, session, result.status);
                let (decision, summary) = if let Some(assessment) = result.assessment {
                    (
                        session.queue_assessment(now, assessment, result.screen.is_some()),
                        Some(session.previous.as_str()),
                    )
                } else {
                    (
                        if result.status == "unavailable" {
                            "unavailable"
                        } else {
                            "failed"
                        },
                        None,
                    )
                };
                let frame_id = result.screen.as_ref().map(|screen| screen.id.as_str());
                let input = match (result.status, frame_id) {
                    ("unavailable", _) => "model_unavailable",
                    (_, Some(_)) => "image",
                    ("image", None) => "no_frame",
                    _ => "model_text_only",
                };
                with_notifier(world, |notifier| {
                    diagnostic(
                        notifier,
                        companion_id,
                        "evaluated",
                        json!({
                            "image": frame_id.is_some(), "input": input, "frame_id": frame_id,
                            "decision": decision, "summary": summary,
                        }),
                    );
                });
                if result.screen.is_some() {
                    session.last_evaluated = result.screen;
                }
            }
            if let Some(entity) = agent::agent_of(world, companion_id) {
                let current =
                    |reaction: &Reaction| now.duration_since(reaction.at) <= OBSERVATION_LIFETIME;
                session.pending_important.retain(current);
                if world.get::<ActiveTurn>(entity).is_none() {
                    session.delivered = None;
                    let next_reaction = session
                        .pending_important
                        .pop_front()
                        .or_else(|| session.pending.take().filter(current))
                        .or_else(|| {
                            (session.task.is_none()
                                && now.duration_since(session.last_delivery)
                                    >= QUIET_HEARTBEAT_INTERVAL)
                                .then(|| Reaction {
                                    summary: session.quiet_summary(),
                                    kind: ReportKind::Heartbeat,
                                    at: now,
                                })
                        });
                    if let Some(reaction) = next_reaction {
                        deliver(world, entity, companion_id, session, reaction, now);
                    }
                }
                if session.dirty && session.task.is_none() && now >= session.next_evaluation {
                    session.next_evaluation = now + EVALUATION_INTERVAL;
                    session.dirty = false;
                    if let Ok(Some(companion)) = store.companion_repo().companion(companion_id) {
                        evaluate(world, entity, &store, &companion, session, now);
                    }
                }
            }
            true
        });
    });
}

pub(crate) fn preview(world: &World, id: &str) -> Result<w::ScreenFrame> {
    let now = Instant::now();
    let hub = world.resource::<DeviceHub>();
    world
        .resource::<Eyes>()
        .sessions
        .values()
        .filter(|session| session.valid(hub, now))
        .flat_map(|session| session.recent_screens.iter().chain(&session.last_evaluated))
        .find(|sample| sample.id == id && now.duration_since(sample.at) <= PREVIEW_LIFETIME)
        .map(|sample| w::ScreenFrame {
            jpeg: sample.jpeg.to_vec(),
            ..Default::default()
        })
        .ok_or_else(|| HostError::NotFound("perception frame expired or unavailable".into()))
}

impl Eyes {
    pub(crate) fn context(
        &self,
        companion: &str,
        author: Option<&Author>,
        hub: &DeviceHub,
    ) -> Option<&Delivered> {
        let request = author.filter(|author| is_eyes(author))?.request.as_ref()?;
        let session = self.sessions.get(companion)?;
        let delivery = session.delivered.as_ref()?;
        (delivery.id == *request && session.valid(hub, Instant::now())).then_some(delivery)
    }
}

pub(crate) fn is_eyes(author: &Author) -> bool {
    matches!(&author.kind, AuthorKind::Plugin(name) if name == AUTHOR)
}

#[cfg(test)]
mod tests;
