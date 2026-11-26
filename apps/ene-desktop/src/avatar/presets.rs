use std::path::{Path, PathBuf};

/// The per-user directory where downloaded model and motion assets are cached.
pub fn asset_cache() -> Option<PathBuf> {
    Some(
        directories::ProjectDirs::from("", "", "ene")?
            .cache_dir()
            .to_owned(),
    )
}

/// Normalizes `.` and `..` segments onto an absolute path without `\\?\` verbatim
/// prefixes so `ene-avatar` (`AssetPath::try_parse`) accepts the path reliably.
pub(super) fn normalize_path(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let mut normalized = PathBuf::new();
    for component in abs.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Prefix(prefix) => {
                if let std::path::Prefix::VerbatimDisk(drive) = prefix.kind() {
                    normalized.push(format!("{}:", char::from(drive)));
                } else {
                    normalized.push(prefix.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

fn user_model_dirs() -> impl Iterator<Item = PathBuf> {
    directories::ProjectDirs::from("", "", "ene")
        .into_iter()
        .flat_map(|dirs| {
            #[cfg(windows)]
            let paths = [
                dirs.data_dir().join("models"),
                dirs.config_dir().with_file_name("models"),
                dirs.data_local_dir().join("models"),
                dirs.config_local_dir().with_file_name("models"),
            ];
            #[cfg(not(windows))]
            let paths = [dirs.data_dir().join("models")];
            paths
        })
}

/// Searches standard development and installed layouts for the bundled `ENE.vrm`.
pub fn bundled_ene_model() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("assets").join("ENE.vrm"));
        candidates.push(
            dir.join("..")
                .join("share")
                .join("ene")
                .join("assets")
                .join("ENE.vrm"),
        );
        candidates.push(
            dir.join("..")
                .join("Resources")
                .join("assets")
                .join("ENE.vrm"),
        );
        candidates.push(dir.join("..").join("assets").join("ENE.vrm"));
        candidates.push(dir.join("..").join("..").join("assets").join("ENE.vrm"));
    }
    candidates.push(PathBuf::from("assets").join("ENE.vrm"));
    candidates.extend(user_model_dirs().map(|dir| dir.join("ENE.vrm")));
    if let Some(cache) = asset_cache() {
        candidates.push(cache.join("models").join("ENE.vrm"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("ENE.vrm"),
    );

    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| normalize_path(&path))
}

/// Resolves a `Companion.body` model reference (`"ENE.vrm"`, `"Seed-san.vrm"`, or an
/// existing `.vrm` path without parent traversal) with fallback to the default model.
pub fn resolve_model_path(spec: Option<&str>, fallback: Option<&Path>) -> Option<PathBuf> {
    if let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) {
        let candidate_path = Path::new(raw);
        let has_parent = candidate_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
        if !has_parent && raw.to_ascii_lowercase().ends_with(".vrm") {
            if candidate_path.is_absolute() && candidate_path.is_file() {
                if let Some(clean) = normalize_path(candidate_path) {
                    return Some(clean);
                }
            } else if candidate_path.components().count() == 1 {
                let file_name = candidate_path.file_name()?;
                if file_name.eq_ignore_ascii_case("ENE.vrm")
                    && let Some(found) = bundled_ene_model()
                {
                    return Some(found);
                }
                let mut dirs = Vec::new();
                if let Some(bundled) =
                    bundled_ene_model().and_then(|p| p.parent().map(Path::to_path_buf))
                {
                    dirs.push(bundled);
                }
                if let Some(cache) = asset_cache() {
                    dirs.push(cache.join("models"));
                }
                dirs.extend(user_model_dirs());
                for dir in dirs {
                    let full = dir.join(file_name);
                    if full.is_file()
                        && let Some(clean) = normalize_path(&full)
                    {
                        return Some(clean);
                    }
                }
            }
        }
    }
    fallback.map(Path::to_path_buf)
}

/// The default model: the bundled `ENE.vrm` first, then the fetched sample cache.
pub fn default_model() -> Option<PathBuf> {
    if let Some(path) = bundled_ene_model() {
        return Some(path);
    }
    let path = asset_cache()?.join("models").join("Seed-san.vrm");
    path.is_file().then_some(path)
}

/// Searches adjacent and Cargo build directories for the `ene-avatar` renderer binary.
pub fn default_renderer() -> Option<PathBuf> {
    let binary = format!("ene-avatar{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        dir.join(&binary),
        dir.join("..").join("release").join(&binary),
        dir.join("..").join("debug").join(&binary),
    ];
    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| normalize_path(&path))
}

pub const MOTIONS: &[(&str, &str)] = &[
    // Existing 11 core motions
    ("idle", "idle-breath.vrma"),
    ("look_around", "idle-look-around.vrma"),
    ("adjust_posture", "guarded-idle.vrma"),
    ("sleepy_nod", "sleepy-nod.vrma"),
    ("nod", "acknowledge.vrma"),
    ("tilt_head", "task-think.vrma"),
    ("stretch", "break-stretch.vrma"),
    ("wave", "welcome-back.vrma"),
    ("ask_question", "ask-question.vrma"),
    ("task_complete", "task-complete.vrma"),
    ("apologize", "apologize.vrma"),
    // Expanded Kimodo DesktopMate & companion motions (24)
    ("idle_sway", "idle-sway.vrma"),
    ("idle_hands_behind", "idle-hands-behind.vrma"),
    ("yawn", "yawn.vrma"),
    ("deep_breath", "deep-breath.vrma"),
    ("shrug", "shrug.vrma"),
    ("bow", "polite-bow.vrma"),
    ("goodbye", "goodbye-wave.vrma"),
    ("peek", "peek-curious.vrma"),
    ("beckon", "beckon.vrma"),
    ("head_pat", "head-pat.vrma"),
    ("surprised", "tap-surprise.vrma"),
    ("shy", "shy-fidget.vrma"),
    ("happy_bounce", "happy-bounce.vrma"),
    ("clap", "clap.vrma"),
    ("cheer", "cheer-encourage.vrma"),
    ("music_groove", "music-groove.vrma"),
    ("sit_idle", "perch-idle.vrma"),
    ("sit_swing_legs", "perch-kick.vrma"),
    ("sit_wave", "perch-greet.vrma"),
    ("drag_held", "drag-held.vrma"),
    ("listen", "listen-active.vrma"),
    ("explain", "speak-explain.vrma"),
    ("shake_head", "disagree.vrma"),
    ("aha", "aha.vrma"),
];

pub const LOOPING_MOTIONS: &[&str] = &[
    "idle",
    "idle_sway",
    "idle_hands_behind",
    "music_groove",
    "sit_idle",
    "sit_swing_legs",
    "drag_held",
];

pub fn is_looping_motion(name: &str) -> bool {
    LOOPING_MOTIONS.contains(&name)
}

pub(super) fn is_persistent_idle(name: &str) -> bool {
    matches!(
        name,
        "idle" | "idle_sway" | "idle_hands_behind" | "sit_idle" | "sit_swing_legs"
    )
}

fn is_seated_motion(name: &str) -> bool {
    matches!(name, "sit_idle" | "sit_swing_legs" | "sit_wave")
}

pub(super) fn idle_return_preset(base: Option<&str>, finished: Option<&str>) -> &'static str {
    if finished.is_some_and(is_seated_motion) {
        return match base {
            Some("sit_swing_legs") => "sit_swing_legs",
            _ => "sit_idle",
        };
    }
    match base {
        Some("idle_sway") => "idle_sway",
        Some("idle_hands_behind") => "idle_hands_behind",
        _ => "idle",
    }
}

/// The selected VRMAs ship beside the bundled ENE model.
pub fn motion(name: &str) -> Option<PathBuf> {
    let (_, file) = MOTIONS.iter().find(|(preset, _)| *preset == name)?;
    let model = bundled_ene_model()?;
    let path = model.parent()?.join("vrma").join(file);
    path.is_file().then_some(path)
}
