use serde::Deserialize;

pub use ene_platform_os::{Observation, Window};

/// A parsed `eye.watch` rule.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    pub target_pattern: String,
    pub match_kind: MatchKind,
    pub notify_on: NotifyOn,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Title,
    AppId,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOn {
    Focus,
    Blur,
    TitleChange,
}

impl Watch {
    /// Whether the observed window matches this rule's target.
    ///
    /// `target_pattern` is documented as "a substring or regular expression"
    /// (`docs/api.md` §知覚), so it is compiled as a case-insensitive pattern
    /// and falls back to a case-insensitive substring when it is not valid
    /// regex. A pattern that matches nothing is the model's mistake, not a
    /// reason to fail the operation.
    pub fn matches(&self, window: &Window) -> bool {
        let subject = match self.match_kind {
            MatchKind::Title => &window.title,
            MatchKind::AppId => &window.app_id,
        };
        match regex::RegexBuilder::new(&self.target_pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(pattern) => pattern.is_match(subject),
            Err(_) => subject
                .to_lowercase()
                .contains(&self.target_pattern.to_lowercase()),
        }
    }
}

/// The sensor's state between samples: what the last observation was, so a
/// rule can fire on a transition rather than on every sample.
#[derive(Default)]
pub struct Sensor {
    /// The rule currently applied by the host, if any.
    pub watch: Option<Watch>,
    /// The window the last sample saw, for focus and title-change edges.
    last: Option<Window>,
    /// Whether the last sample matched, so `focus` fires once per entry.
    matched: bool,
    idle: bool,
}

/// What a sample produced for the host.
pub enum Report {
    /// The foreground window changed.
    Window(Window),
    /// The user's activity state changed.
    Activity { is_active: bool, idle_seconds: u64 },
    /// A watch rule matched on this transition.
    Interrupt(Window, NotifyOn),
}

impl Sensor {
    /// Applies a rule the host dispatched. `None` clears it.
    pub fn apply(&mut self, watch: Option<Watch>) {
        self.watch = watch;
        self.matched = false;
    }

    /// Turns one observation into the reports the host should receive.
    ///
    /// A changed window is always reported, because the host summarizes it for
    /// the conversation. A matching rule adds an interrupt, so the parent is
    /// woken without waiting for the next periodic sample.
    pub fn observe(&mut self, observation: Observation) -> Vec<Report> {
        let mut reports = Vec::new();
        if let Some(window) = observation.window {
            let changed = self.last.as_ref() != Some(&window);
            if changed {
                reports.push(Report::Window(window.clone()));
            }
            if let Some(watch) = self.watch.as_ref().filter(|watch| watch.enabled) {
                let matched = watch.matches(&window);
                let fires = match watch.notify_on {
                    NotifyOn::Focus => matched && !self.matched,
                    NotifyOn::Blur => !matched && self.matched,
                    NotifyOn::TitleChange => {
                        matched
                            && self.last.as_ref().is_some_and(|last| {
                                last.app_id == window.app_id && last.title != window.title
                            })
                    }
                };
                if fires {
                    let target = match watch.notify_on {
                        NotifyOn::Blur => self.last.as_ref().unwrap_or(&window),
                        _ => &window,
                    };
                    reports.push(Report::Interrupt(target.clone(), watch.notify_on));
                }
                self.matched = matched;
            }
            self.last = Some(window);
        }
        let idle = observation.idle_seconds >= IDLE_THRESHOLD;
        if idle != self.idle {
            reports.push(Report::Activity {
                is_active: !idle,
                idle_seconds: observation.idle_seconds,
            });
        }
        self.idle = idle;
        reports
    }
}

/// The idle threshold the design names for the activity event.
pub const IDLE_THRESHOLD: u64 = 60;
