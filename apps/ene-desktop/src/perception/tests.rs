use super::rules::MatchKind;
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
            .any(|report| matches!(report, Report::Interrupt(_, _))),
        "entering the match interrupts"
    );
    let second = sensor.observe(github);
    assert!(
        !second
            .iter()
            .any(|report| matches!(report, Report::Interrupt(_, _))),
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
            .any(|report| matches!(report, Report::Interrupt(window, NotifyOn::Blur) if window.title == "GitHub")),
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
            .any(|report| matches!(report, Report::Interrupt(_, _)))
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
    assert!(matches!(
        sensor
            .observe(Observation {
                window: None,
                idle_seconds: 0
            })
            .as_slice(),
        [Report::Activity {
            is_active: true,
            ..
        }]
    ));
}
