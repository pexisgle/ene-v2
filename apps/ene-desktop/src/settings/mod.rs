//! Client settings persistence and editable state.
mod persistence;
pub use persistence::{ClientSettings, config_path};
use std::path::Path;

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
            model: value.avatar.model.clone(),
            motion: value.avatar.motion.clone().unwrap_or_default(),
            motion_playing: value.avatar.motion_playing,
            motion_loop: value.avatar.motion_loop,
            fps_index: i32::from(value.avatar.fps == 60),
            size_text: value.avatar.size_percent.to_string(),
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
    pub fn merged(&self) -> Result<ClientSettings, ValidationError> {
        let d = &self.draft;
        let b = &self.baseline;
        let mut value = self.saved.clone();
        macro_rules! changed {
            ($field:ident, $target:expr) => {
                if d.$field != b.$field {
                    $target = d.$field.clone();
                }
            };
        }
        if d.font_family.trim().is_empty() {
            return Err(ValidationError {
                field: "font",
                message: "フォント名を入力してください。".into(),
            });
        }
        let size = d
            .size_text
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|v| (50..=200).contains(v));
        let size = size.ok_or_else(|| ValidationError {
            field: "size",
            message: "キャラクターの大きさは50〜200の整数で入力してください。".into(),
        })?;
        changed!(font_family, value.font_family);
        changed!(text_size, value.text_size);
        changed!(theme_index, value.theme_index);
        changed!(model, value.avatar.model);
        changed!(motion_playing, value.avatar.motion_playing);
        changed!(motion_loop, value.avatar.motion_loop);
        if d.motion != b.motion {
            value.avatar.motion = (!d.motion.is_empty()).then(|| d.motion.clone());
        }
        if d.fps_index != b.fps_index {
            value.avatar.fps = if d.fps_index == 1 { 60 } else { 30 };
        }
        if d.size_text != b.size_text {
            value.avatar.size_percent = size;
        }
        value.theme_index = value.theme_index.clamp(0, 2);
        value.text_size = value.text_size.clamp(12, 28);
        Ok(value)
    }
    /// Publish a successfully persisted configuration without overwriting edits.
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
    pub fn save_draft(&mut self, path: &Path) -> Result<(), ValidationError> {
        let updated = self.merged()?;
        updated
            .save(path)
            .map_err(|message| ValidationError { field: "", message })?;
        self.saved = updated;
        self.reset();
        Ok(())
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
