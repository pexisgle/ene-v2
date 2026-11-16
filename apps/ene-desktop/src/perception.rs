//! The "eyes": the active window and the user's idle state.
//!
//! The client samples the OS and reports what it observed. It never guesses:
//! on a platform where the window cannot be read, the sensor reports nothing
//! and the host keeps its previous summary (`docs/design/desktop-client.md` §2.2).
//!
//! `eye.watch` rules are applied here, not in the host: the host forwards the
//! rule as a `DeviceOperation` and does not interpret it.

use serde::Deserialize;
use serde_json::Value;

/// What the sensor can observe right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The foreground window's title and process name, when readable.
    pub window: Option<Window>,
    /// Seconds since the last input, and whether the user counts as active.
    pub idle_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub title: String,
    pub app_id: String,
}

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
    /// Parses a rule from a `device_request` or `set_eye_watch` input.
    pub fn parse(input: &Value) -> Result<Self, String> {
        serde_json::from_value(input.clone()).map_err(|error| error.to_string())
    }

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
}

/// What a sample produced for the host.
pub enum Report {
    /// The foreground window changed.
    Window(Window),
    /// The user's activity state changed.
    Activity { is_active: bool, idle_seconds: u64 },
    /// A watch rule matched on this transition.
    Interrupt(Window),
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
        if let Some(window) = observation.window.clone() {
            let changed = self.last.as_ref() != Some(&window);
            if changed {
                reports.push(Report::Window(window.clone()));
                self.last = Some(window.clone());
            }
            if let Some(watch) = self.watch.as_ref().filter(|watch| watch.enabled) {
                let matched = watch.matches(&window);
                let fires = match watch.notify_on {
                    NotifyOn::Focus => matched && !self.matched,
                    NotifyOn::Blur => !matched && self.matched,
                    NotifyOn::TitleChange => matched && changed,
                };
                if fires {
                    reports.push(Report::Interrupt(window));
                }
                self.matched = matched;
            }
        }
        if observation.idle_seconds >= IDLE_THRESHOLD {
            reports.push(Report::Activity {
                is_active: false,
                idle_seconds: observation.idle_seconds,
            });
        }
        reports
    }
}

/// The idle threshold the design names for the activity event.
pub const IDLE_THRESHOLD: u64 = 60;

/// Samples the OS once.
#[cfg(windows)]
pub fn observe() -> Observation {
    windows_impl::observe()
}

/// A platform without a window hook reports no window, and says so by
/// returning `None` rather than inventing one.
#[cfg(not(windows))]
pub fn observe() -> Observation {
    Observation {
        window: None,
        idle_seconds: 0,
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{Observation, Window};
    use windows::Win32::{
        Foundation::{CloseHandle, HWND, MAX_PATH},
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::{
            Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
            WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId},
        },
    };

    pub(super) fn observe() -> Observation {
        Observation {
            window: foreground(),
            idle_seconds: idle_seconds(),
        }
    }

    /// The foreground window's title and process name. Either may be empty:
    /// a window the OS will not describe is reported as it is, not guessed.
    fn foreground() -> Option<Window> {
        // SAFETY: the calls read OS state into locals; every handle is closed.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == HWND::default() {
                return None;
            }
            let mut buffer = [0u16; MAX_PATH as usize];
            let length = GetWindowTextW(hwnd, &mut buffer);
            let title = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let app_id = process_name(pid).unwrap_or_default();
            if title.is_empty() && app_id.is_empty() {
                return None;
            }
            Some(Window { title, app_id })
        }
    }

    /// The executable's file name, which is the app id a rule matches on.
    fn process_name(pid: u32) -> Option<String> {
        // SAFETY: the handle is closed on every path below.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = [0u16; MAX_PATH as usize];
            let mut size = buffer.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            path.rsplit(['\\', '/']).next().map(ToOwned::to_owned)
        }
    }

    fn idle_seconds() -> u64 {
        // SAFETY: `LASTINPUTINFO` is a plain struct filled by the OS.
        unsafe {
            let mut info = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            if !GetLastInputInfo(&mut info).as_bool() {
                return 0;
            }
            let now = windows::Win32::System::SystemInformation::GetTickCount();
            u64::from(now.wrapping_sub(info.dwTime)) / 1000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(title: &str, app_id: &str) -> Window {
        Window {
            title: title.into(),
            app_id: app_id.into(),
        }
    }

    fn rule(match_kind: MatchKind, notify_on: NotifyOn) -> Watch {
        Watch {
            target_pattern: "GitHub".into(),
            match_kind,
            notify_on,
            enabled: true,
        }
    }

    #[test]
    fn a_rule_matches_a_substring_of_its_chosen_subject() {
        let by_title = rule(MatchKind::Title, NotifyOn::Focus);
        assert!(by_title.matches(&window("GitHub - ene", "chrome.exe")));
        assert!(!by_title.matches(&window("editor", "github.exe")));
        let by_app = rule(MatchKind::AppId, NotifyOn::Focus);
        assert!(by_app.matches(&window("anything", "github.exe")));
        assert!(!by_app.matches(&window("GitHub", "chrome.exe")));
    }

    /// The declared schema promises a regular expression, and the design's own
    /// example is `.*\.rs`.
    #[test]
    fn a_rule_accepts_a_regular_expression() {
        let rust_file = Watch {
            target_pattern: r".*\.rs".into(),
            match_kind: MatchKind::Title,
            notify_on: NotifyOn::Focus,
            enabled: true,
        };
        assert!(rust_file.matches(&window("main.rs - ene-v2", "code.exe")));
        assert!(!rust_file.matches(&window("README.md", "code.exe")));
    }

    /// A pattern that is not valid regex still works as a substring instead of
    /// matching nothing.
    #[test]
    fn an_invalid_pattern_falls_back_to_a_substring() {
        let unbalanced = Watch {
            target_pattern: "a(b".into(),
            match_kind: MatchKind::Title,
            notify_on: NotifyOn::Focus,
            enabled: true,
        };
        assert!(unbalanced.matches(&window("x a(b y", "app")));
        assert!(!unbalanced.matches(&window("nothing", "app")));
    }

    #[test]
    fn a_rule_parses_only_from_a_complete_declaration() {
        let complete = serde_json::json!({
            "target_pattern": "GitHub",
            "match_kind": "title",
            "notify_on": "focus",
            "enabled": true,
        });
        assert!(Watch::parse(&complete).is_ok());
        let mut incomplete = complete.clone();
        incomplete.as_object_mut().unwrap().remove("notify_on");
        assert!(Watch::parse(&incomplete).is_err());
        let mut unknown = complete;
        unknown["extra"] = serde_json::json!(1);
        assert!(Watch::parse(&unknown).is_err());
    }

    /// The parent must be woken on the transition into a match, not on every
    /// sample while the match holds.
    #[test]
    fn a_focus_rule_fires_once_per_entry() {
        let mut sensor = Sensor::default();
        sensor.apply(Some(rule(MatchKind::Title, NotifyOn::Focus)));
        let github = Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        };
        let other = Observation {
            window: Some(window("editor", "code.exe")),
            idle_seconds: 0,
        };
        assert!(matches!(
            sensor.observe(other).as_slice(),
            [Report::Window(_)]
        ));
        let first = sensor.observe(github.clone());
        assert!(
            first
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_))),
            "entering the match interrupts"
        );
        let second = sensor.observe(github);
        assert!(
            !second
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_))),
            "staying in the match does not interrupt again"
        );
    }

    #[test]
    fn a_blur_rule_fires_when_the_match_is_left() {
        let mut sensor = Sensor::default();
        sensor.apply(Some(rule(MatchKind::Title, NotifyOn::Blur)));
        let github = Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        };
        let other = Observation {
            window: Some(window("editor", "code.exe")),
            idle_seconds: 0,
        };
        sensor.observe(github);
        let left = sensor.observe(other);
        assert!(
            left.iter()
                .any(|report| matches!(report, Report::Interrupt(_))),
            "leaving the match interrupts"
        );
    }

    #[test]
    fn a_disabled_rule_never_interrupts() {
        let mut sensor = Sensor::default();
        let mut disabled = rule(MatchKind::Title, NotifyOn::Focus);
        disabled.enabled = false;
        sensor.apply(Some(disabled));
        let reports = sensor.observe(Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        });
        assert!(
            !reports
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_)))
        );
    }

    #[test]
    fn an_idle_observation_reports_the_activity_state() {
        let mut sensor = Sensor::default();
        let reports = sensor.observe(Observation {
            window: None,
            idle_seconds: IDLE_THRESHOLD,
        });
        assert!(matches!(
            reports.as_slice(),
            [Report::Activity {
                is_active: false,
                ..
            }]
        ));
    }
}
