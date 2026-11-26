//! Turning perception events from a client stream into session state.
//!
//! Every recorder mutates the one [`Session`] the event's stream belongs to and
//! returns an optional diagnostic for the client to see; nothing here calls the
//! model, so a report is cheap enough to handle inline on the app thread.
use super::{
    Eyes, PREVIEW_FRAMES, Reaction, ReportKind, ScreenSample, Session, delivery::diagnostic,
    valid_jpeg,
};
use crate::host::{
    error::{HostError, Result},
    notify::Notifier,
    resources::Perception,
};
use ene_rpc as w;
use ene_store::util::truncate;
use serde_json::{Value, json};
use std::time::Instant;

/// Applies a perception event to `companion`.
///
/// The event handling itself: pure over the three maps it is given.
pub(crate) fn apply_report(
    eyes: &mut Eyes,
    perception: &mut Perception,
    notifier: Option<&Notifier>,
    companion: &str,
    event: w::PerceptionEvent,
) -> Result<()> {
    let Some(notifier) = notifier else {
        return Ok(());
    };
    let now = Instant::now();
    let Some(event_type) = event.event else {
        return Ok(());
    };
    if let w::perception_event::Event::Enabled(enabled) = event_type {
        handle_enabled_event(
            eyes,
            perception,
            notifier,
            EnabledTarget {
                companion,
                stream: StreamId {
                    client: event.client_id,
                    subscription: event.subscription_id,
                },
                enabled,
                now,
            },
        );
        return Ok(());
    }
    let outcome = {
        let Some(session) = eyes
            .sessions
            .get_mut(companion)
            .filter(|s| s.subscription == event.subscription_id)
        else {
            return Ok(());
        };
        session.seen = now;
        apply_session_event(session, event_type, now)?
    };
    match outcome {
        SessionOutcome::Ignore => {}
        SessionOutcome::WatchMatched(transition) => {
            diagnostic(
                notifier,
                companion,
                "watch_matched",
                &json!({"transition": transition}),
            );
        }
        SessionOutcome::Diagnostic(diag) => {
            if let Some((action, detail)) = diag {
                diagnostic(notifier, companion, action, &detail);
            }
        }
    }
    Ok(())
}

enum SessionOutcome {
    Ignore,
    WatchMatched(&'static str),
    Diagnostic(Option<(&'static str, Value)>),
}

/// The identity of the stream an enable/disable event arrived on.
struct StreamId {
    client: String,
    subscription: String,
}

/// What `handle_enabled_event` mutates besides the two maps.
struct EnabledTarget<'a> {
    companion: &'a str,
    stream: StreamId,
    enabled: bool,
    now: Instant,
}

fn handle_enabled_event(
    eyes: &mut Eyes,
    perception: &mut Perception,
    notifier: &Notifier,
    target: EnabledTarget<'_>,
) {
    let EnabledTarget {
        companion,
        stream,
        enabled,
        now,
    } = target;
    let StreamId {
        client: client_id,
        subscription: subscription_id,
    } = stream;
    let diag_action = {
        let same_stream = eyes
            .sessions
            .get(companion)
            .is_some_and(|session| session.subscription == subscription_id);
        if enabled {
            if same_stream {
                if let Some(session) = eyes.sessions.get_mut(companion) {
                    session.seen = now;
                }
                None
            } else {
                eyes.sessions.insert(
                    companion.to_string(),
                    Session::new(client_id, subscription_id, now),
                );
                Some("enabled")
            }
        } else if same_stream {
            eyes.sessions.remove(companion);
            Some("paused")
        } else {
            None
        }
    };
    if let Some(action) = diag_action {
        diagnostic(notifier, companion, action, &json!({}));
    }
    if enabled {
        perception.events.remove(companion);
    }
}

fn apply_session_event(
    session: &mut Session,
    event_type: w::perception_event::Event,
    now: Instant,
) -> Result<SessionOutcome> {
    use w::perception_event::Event;
    let diag = match event_type {
        Event::ActiveWindow(window) => record_window_event(session, &window, now),
        Event::UserActivity(activity) => record_activity_event(session, &activity, now),
        Event::PerceptionSummary(summary) => {
            session.event(now, json!({"sensor": truncate(&summary.summary, 1000)}));
            None
        }
        Event::ScreenFrame(frame) => record_screen_frame(session, *frame, now)?,
        Event::WatchMatch(watch) => return Ok(apply_watch_match(session, &watch, now)),
        Event::AvatarInteraction(interaction) => {
            record_avatar_interaction(session, &interaction, now)
        }
        Event::Enabled(_) => unreachable!(),
    };
    session.dirty = true;
    Ok(SessionOutcome::Diagnostic(diag))
}

fn record_window_event(
    session: &mut Session,
    window: &w::ActiveWindowChanged,
    now: Instant,
) -> Option<(&'static str, Value)> {
    let window = window_data(window);
    if session.window.as_ref() != Some(&window) {
        session.event(now, json!({"window": window}));
        session.window = Some(window);
        Some(("window_changed", json!({})))
    } else {
        None
    }
}

fn record_activity_event(
    session: &mut Session,
    activity: &w::UserActivityState,
    now: Instant,
) -> Option<(&'static str, Value)> {
    let changed = session
        .activity
        .is_none_or(|(active, _)| active != activity.is_active);
    if changed && session.activity.is_some() {
        session.event(now, json!({"active": activity.is_active}));
    }
    session.activity = Some((activity.is_active, activity.idle_seconds));
    changed.then(|| {
        (
            "activity_changed",
            json!({
                "active": activity.is_active, "idle_seconds": activity.idle_seconds,
            }),
        )
    })
}

fn record_screen_frame(
    session: &mut Session,
    frame: w::ScreenFrame,
    now: Instant,
) -> Result<Option<(&'static str, Value)>> {
    if !valid_jpeg(&frame.jpeg) {
        return Err(HostError::Invalid("invalid perception JPEG".into()));
    }
    let size = frame.jpeg.len();
    let sample = ScreenSample {
        id: uuid::Uuid::new_v4().to_string(),
        at: now,
        jpeg: frame.jpeg.into(),
    };
    let detail = json!({"bytes": size, "frame_id": sample.id});
    session.screen = Some(sample.clone());
    if session.recent_screens.len() == PREVIEW_FRAMES {
        session.recent_screens.pop_front();
    }
    session.recent_screens.push_back(sample);
    Ok(Some(("screen_received", detail)))
}

fn apply_watch_match(session: &mut Session, watch: &w::WatchMatch, now: Instant) -> SessionOutcome {
    let Some(window) = watch.window.as_option() else {
        return SessionOutcome::Ignore;
    };
    let transition = match watch.transition.as_known() {
        Some(w::WatchTransition::Focus) => "前面になった",
        Some(w::WatchTransition::Blur) => "前面から外れた",
        Some(w::WatchTransition::TitleChange) => "タイトルが変わった",
        _ => return SessionOutcome::Ignore,
    };
    let target = [window.title.trim(), window.app_id.trim()]
        .into_iter()
        .find(|label| !label.is_empty())
        .unwrap_or("監視対象");
    session.pending = Some(Reaction {
        summary: format!("監視条件に一致。{transition} · {}", truncate(target, 220)),
        kind: ReportKind::Watch,
        at: now,
    });
    SessionOutcome::WatchMatched(transition)
}

fn record_avatar_interaction(
    session: &mut Session,
    interaction: &w::AvatarInteraction,
    now: Instant,
) -> Option<(&'static str, Value)> {
    let kind_label = match interaction.kind.as_known() {
        Some(w::AvatarInteractionKind::DoubleClick) => "ダブルクリック",
        Some(w::AvatarInteractionKind::Drag) => "ドラッグ",
        _ => "クリック",
    };
    session.pending = Some(Reaction {
        summary: format!("ユーザーがアバターを{kind_label}しました"),
        kind: ReportKind::Automatic,
        at: now,
    });
    Some((
        "avatar_interaction",
        json!({"kind": kind_label, "region": interaction.region}),
    ))
}

fn window_data(window: &w::ActiveWindowChanged) -> Value {
    json!({"title": truncate(&window.title, 240), "app": truncate(&window.app_id, 120)})
}
