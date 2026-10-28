//! Client settings persistence and editable state.
pub use crate::infrastructure::settings::{ClientSettings, config_path};

/// Editable values intentionally exclude position and visibility (live actions).
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub font_family: String,
    pub text_size: i32,
    pub theme_index: i32,
    pub model: String,
    pub motion: String,
    pub motion_playing: bool,
    pub motion_loop: bool,
    pub fps_index: i32,
    pub size_text: String,
}
impl From<&ClientSettings> for Draft {
    fn from(value: &ClientSettings) -> Self {
        Self {
            font_family: value.font_family.clone(),
            text_size: value.text_size,
            theme_index: value.theme_index,
            model: value.avatar.config.model.clone(),
            motion: value.avatar.config.motion.clone().unwrap_or_default(),
            motion_playing: value.avatar.config.motion_playing,
            motion_loop: value.avatar.config.motion_loop,
            fps_index: i32::from(value.avatar.fps == 60),
            size_text: value.avatar.config.size_percent.to_string(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

pub struct State {
    pub saved: ClientSettings,
    pub draft: Draft,
    baseline: Draft,
    /// Invalidates asynchronous selections after reset, save, or a newer edit.
    pub generation: u64,
}
impl State {
    pub fn new(saved: ClientSettings) -> Self {
        let draft = Draft::from(&saved);
        Self {
            saved,
            baseline: draft.clone(),
            draft,
            generation: 0,
        }
    }
    pub fn dirty(&self) -> bool {
        self.draft != self.baseline
    }
    pub fn edit(&mut self, draft: Draft) {
        self.draft = draft;
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn reset(&mut self) {
        self.baseline = Draft::from(&self.saved);
        self.draft = self.baseline.clone();
        self.generation = self.generation.wrapping_add(1);
    }
    /// Apply every valid field independently, retaining invalid text in the draft.
    pub fn live_values(&self) -> (ClientSettings, Vec<ValidationError>) {
        let d = &self.draft;
        let b = &self.baseline;
        let mut value = self.saved.clone();
        let mut errors = Vec::new();
        macro_rules! changed {
            ($field:ident, $target:expr) => {
                if d.$field != b.$field {
                    $target = d.$field.clone();
                }
            };
        }
        if d.font_family.trim().is_empty() {
            errors.push(ValidationError {
                field: "font",
                message: "フォント名を入力してください。".into(),
            });
        } else {
            changed!(font_family, value.font_family);
        }
        if let Some(size) = d
            .size_text
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|v| (50..=200).contains(v))
        {
            if d.size_text != b.size_text {
                value.avatar.config.size_percent = size;
            }
        } else {
            errors.push(ValidationError {
                field: "size",
                message: "50〜200の整数で入力してください。".into(),
            });
        }
        changed!(text_size, value.text_size);
        changed!(theme_index, value.theme_index);
        changed!(model, value.avatar.config.model);
        changed!(motion_playing, value.avatar.config.motion_playing);
        changed!(motion_loop, value.avatar.config.motion_loop);
        if d.motion != b.motion {
            value.avatar.config.motion = (!d.motion.is_empty()).then(|| d.motion.clone());
        }
        if d.fps_index != b.fps_index {
            value.avatar.fps = if d.fps_index == 1 { 60 } else { 30 };
        }
        value.theme_index = value.theme_index.clamp(0, 2);
        value.text_size = value.text_size.clamp(12, 28);
        (value, errors)
    }
    /// Publish an accepted live configuration without overwriting edits.
    pub fn accept_live(&mut self, value: ClientSettings) {
        let next = Draft::from(&value);
        macro_rules! refresh { ($($field:ident),* $(,)?) => { $(
            if self.draft.$field == self.baseline.$field { self.draft.$field = next.$field.clone(); }
        )* }; }
        refresh!(
            font_family,
            text_size,
            theme_index,
            model,
            motion,
            motion_playing,
            motion_loop,
            fps_index,
            size_text
        );
        self.baseline = next;
        self.saved = value;
    }
    /// Ignore both successful and failed picker results superseded by a draft change.
    pub fn selection_result(
        &mut self,
        generation: u64,
        motion: bool,
        result: Result<Option<String>, String>,
    ) -> Result<bool, String> {
        if generation != self.generation {
            return Ok(false);
        }
        let Some(path) = result? else {
            return Ok(false);
        };
        if motion {
            self.draft.motion = path;
            self.draft.motion_playing = true;
        } else {
            self.draft.model = path;
        }
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_fields_do_not_block_valid_edits_or_reset_placement() {
        let mut state = State::new(ClientSettings::default());
        let mut draft = state.draft.clone();
        draft.size_text = "2".into();
        draft.font_family = "".into();
        draft.theme_index = 2;
        state.edit(draft);
        let (mut live, errors) = state.live_values();
        assert_eq!(errors.len(), 2);
        assert_eq!(live.theme_index, 2);
        assert_eq!(live.avatar.config.size_percent, 100);
        live.placement.position = Some([80, 90]);
        state.accept_live(live);
        assert_eq!(state.draft.size_text, "2");
        assert_eq!(state.draft.font_family, "");
        let mut draft = state.draft.clone();
        draft.size_text = "170".into();
        draft.font_family = "monospace".into();
        state.edit(draft);
        let (live, errors) = state.live_values();
        assert!(errors.is_empty());
        assert_eq!(live.avatar.config.size_percent, 170);
        assert_eq!(live.placement.position, Some([80, 90]));
        state.accept_live(live);
        assert!(!state.dirty());
    }

    #[test]
    fn cancelled_and_superseded_selections_preserve_live_values() {
        let mut state = State::new(ClientSettings::default());
        let original = state.saved.clone();
        assert!(!state.selection_result(0, false, Ok(None)).unwrap());
        state.edit(state.draft.clone());
        assert!(
            !state
                .selection_result(0, false, Ok(Some("old.vrm".into())))
                .unwrap()
        );
        assert_eq!(state.saved, original);
        assert!(
            state
                .selection_result(state.generation, true, Ok(Some("new.vrma".into())))
                .unwrap()
        );
        let (live, _) = state.live_values();
        assert_eq!(live.avatar.config.motion.as_deref(), Some("new.vrma"));
    }
}
