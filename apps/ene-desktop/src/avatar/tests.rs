use super::client::{PendingReplies, lip_sync_line, read_loop};
use super::presets::{LOOPING_MOTIONS, idle_return_preset, is_looping_motion, normalize_path};
use super::*;
use ene_avatar_ipc::{AvatarEvent, AvatarMessage, CharacterConfig, Command};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};

#[test]
fn a_reply_and_a_notification_are_distinguished() {
    let reply: AvatarMessage = serde_json::from_str(r#"{"id":1,"ok":null}"#).unwrap();
    assert!(matches!(reply, AvatarMessage::Reply { id: 1, .. }));
    let failed: AvatarMessage =
        serde_json::from_str(r#"{"id":2,"error":"unknown character"}"#).unwrap();
    assert!(matches!(
        failed,
        AvatarMessage::Reply { error: Some(_), .. }
    ));
    let event: AvatarMessage =
        serde_json::from_str(r#"{"event":"state","character":"ene","status":"ready"}"#).unwrap();
    assert!(matches!(event, AvatarMessage::Event(_)));
}

#[test]
fn bundled_presets_resolve_to_vrma_files() {
    use std::collections::BTreeSet;

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/vrma");
    let shipped: BTreeSet<_> = assets
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let selected: BTreeSet<std::ffi::OsString> =
        MOTIONS.iter().map(|&(_, file)| file.into()).collect();
    assert_eq!(shipped, selected);

    for &(preset, _) in MOTIONS {
        let path = motion(preset).expect(preset);
        assert!(path.is_absolute() && path.is_file(), "{path:?}");
    }
    assert!(motion("no-such-motion").is_none());
}

#[test]
fn looping_postures_and_idle_return_preserve_resting_state() {
    assert!(is_looping_motion("idle"));
    assert!(is_looping_motion("idle_sway"));
    assert!(is_looping_motion("idle_hands_behind"));
    assert!(is_looping_motion("sit_idle"));
    assert!(is_looping_motion("sit_swing_legs"));
    assert!(is_looping_motion("music_groove"));
    assert!(is_looping_motion("drag_held"));
    assert!(!is_looping_motion("nod"));
    assert!(!is_looping_motion("sit_wave"));

    for &preset in LOOPING_MOTIONS {
        assert!(
            MOTIONS.iter().any(|&(name, _)| name == preset),
            "looping preset {preset} must be declared in MOTIONS"
        );
    }

    assert_eq!(idle_return_preset(None, Some("nod")), "idle");
    assert_eq!(
        idle_return_preset(Some("idle_sway"), Some("nod")),
        "idle_sway"
    );
    assert_eq!(
        idle_return_preset(Some("idle_hands_behind"), Some("clap")),
        "idle_hands_behind"
    );
    assert_eq!(
        idle_return_preset(Some("sit_idle"), Some("sit_wave")),
        "sit_idle"
    );
    assert_eq!(
        idle_return_preset(Some("sit_swing_legs"), Some("sit_wave")),
        "sit_swing_legs"
    );
    // A standing gesture played from a seated base returns to standing idle.
    assert_eq!(idle_return_preset(Some("sit_idle"), Some("wave")), "idle");
}

/// The renderer's `set` replaces the whole configuration, so changing the
/// motion must carry the placement that was already there.
#[test]
fn changing_a_motion_keeps_the_placement_fields() {
    let mut shown = CharacterConfig::for_model("/models/ene.vrm");
    shown.monitor = Some("eDP-1".into());
    shown.position = Some([100., 200.]);
    shown.size_percent = 140.;
    let mut next = shown.clone();
    next.motion = Some("/motions/nod.vrma".into());
    let config = serde_json::to_value(&next).unwrap();
    assert_eq!(config["monitor"], "eDP-1");
    assert_eq!(config["position"][1], 200.0);
    assert_eq!(config["size_percent"], 140.0);
    assert_eq!(config["motion"], "/motions/nod.vrma");
}

#[test]
fn a_face_request_can_be_set_and_cleared() {
    let set = serde_json::to_value(Command::Expression {
        character: "ene".into(),
        name: Some("happy".into()),
    })
    .unwrap();
    assert_eq!(set["character"], "ene");
    assert_eq!(set["name"], "happy");
    let clear = serde_json::to_value(Command::Expression {
        character: "ene".into(),
        name: None,
    })
    .unwrap();
    assert!(clear["name"].is_null());
}

#[test]
fn the_lip_line_is_one_way_and_clamped() {
    let line = lip_sync_line("ene", 2.5).unwrap();
    assert!(line.ends_with('\n'));
    let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(parsed["op"], "lip_sync");
    assert_eq!(parsed["character"], "ene");
    assert_eq!(parsed["rms"], 1.0);
    assert!(parsed.get("id").is_none());

    let parsed: Value =
        serde_json::from_str(lip_sync_line("ene", -1.0).unwrap().trim_end()).unwrap();
    assert_eq!(parsed["rms"], 0.0);
}

#[test]
fn scaling_keeps_motion_and_placement_fields() {
    let mut shown = CharacterConfig::for_model("/models/ene.vrm");
    shown.motion = Some("/motions/nod.vrma".into());
    shown.monitor = Some("eDP-1".into());
    shown.position = Some([120., 240.]);
    let scaled = shown.with_scale(250.0);
    assert_eq!(scaled.size_percent, 200.0);
    assert_eq!(scaled.motion.as_deref(), Some("/motions/nod.vrma"));
    assert_eq!(scaled.monitor.as_deref(), Some("eDP-1"));
    assert_eq!(scaled.position, Some([120., 240.]));

    let min_scaled = shown.with_scale(10.0);
    assert_eq!(min_scaled.size_percent, 50.0);
}

#[test]
fn b09_read_loop_resolves_pending_and_preserves_all_event_kinds() {
    let input = concat!(
        r#"{"event":"click","character":"ene"}"#,
        "\n",
        r#"{"event":"state","character":"ene","status":"ready"}"#,
        "\n",
        r#"{"event":"finished","character":"ene"}"#,
        "\n",
        r#"{"id":7,"ok":{"status":"done"}}"#,
        "\n"
    );
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let pending: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
    let (reply_tx, mut reply_rx) = oneshot::channel();
    pending.lock().unwrap().insert(7, reply_tx);
    let (waiter_tx, mut waiter_rx) = oneshot::channel();
    pending.lock().unwrap().insert(8, waiter_tx);

    read_loop(input.as_bytes(), events_tx, Arc::clone(&pending));

    let mut events = Vec::new();
    while let Ok(ev) = events_rx.try_recv() {
        events.push(ev);
    }
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], AvatarEvent::Click { .. }));
    assert!(matches!(events[1], AvatarEvent::State { .. }));
    assert!(matches!(events[2], AvatarEvent::Finished { .. }));
    assert_eq!(
        reply_rx.try_recv().unwrap().unwrap(),
        serde_json::json!({"status":"done"})
    );
    assert!(
        waiter_rx.try_recv().unwrap().is_err(),
        "EOF must wake unfinished pending waiters"
    );
}

#[test]
fn normalize_path_cleans_parent_segments_without_verbatim_prefix() {
    let normalized = normalize_path(Path::new("assets/../assets/ENE.vrm")).unwrap();
    let text = normalized.to_string_lossy();
    assert!(
        !text.starts_with(r"\\?\"),
        "unexpected verbatim prefix: {text}"
    );
    assert!(text.ends_with("ENE.vrm"), "unexpected suffix: {text}");
    assert!(
        !normalized.components().any(|c| matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "unnormalized components remained: {normalized:?}"
    );
}

#[test]
fn bundled_ene_model_resolves_workspace_asset() {
    let workspace_asset = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets")
        .join("ENE.vrm");
    if workspace_asset.is_file() {
        let found = bundled_ene_model().expect("bundled ENE.vrm resolves");
        assert!(found.is_file(), "resolved path does not exist: {found:?}");
        assert!(
            found.is_absolute(),
            "resolved path is not absolute: {found:?}"
        );
        assert_eq!(found.file_name().and_then(|n| n.to_str()), Some("ENE.vrm"));
    }
}
