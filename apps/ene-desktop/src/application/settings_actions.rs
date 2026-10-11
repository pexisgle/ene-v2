use super::*;

impl Application {
    pub(in crate::application) fn publish_settings(&self, ui: &AppWindow) {
        let state = self.settings.borrow();
        let view = ui.global::<UiState>();
        view.set_font_family(state.saved.font_family.clone().into());
        view.set_text_size(state.saved.text_size);
        view.set_avatar_visible(state.saved.avatar.visible);
        view.set_motion_playing(state.saved.avatar.config.motion_playing);
        view.set_motion_loop(state.saved.avatar.config.motion_loop);
        view.set_motion_configured(state.saved.avatar.config.motion.is_some());
        view.set_motion_saved(
            state
                .saved
                .avatar
                .config
                .motion
                .clone()
                .unwrap_or_default()
                .into(),
        );
        view.set_settings_draft(project(&state.draft));
        view.set_settings_dirty(state.dirty());
        ui.global::<Theme>().invoke_apply(state.saved.theme_index);
        if let Some(tray) = &self.tray {
            tray.set_avatar_visible(state.saved.avatar.visible);
        }
    }
    pub fn edit_settings(&self, ui: &AppWindow) {
        let draft = read_draft(ui.global::<UiState>().get_settings_draft());
        self.settings.borrow_mut().edit(draft);
        self.apply_draft(ui);
    }
    fn apply_draft(&self, ui: &AppWindow) {
        let (updated, errors) = self.settings.borrow().live_values();
        let view = ui.global::<UiState>();
        view.set_font_error(
            errors
                .iter()
                .find(|e| e.field == "font")
                .map(|e| e.message.clone())
                .unwrap_or_default()
                .into(),
        );
        view.set_size_error(
            errors
                .iter()
                .find(|e| e.field == "size")
                .map(|e| e.message.clone())
                .unwrap_or_default()
                .into(),
        );
        self.persist_live(ui, updated);
    }
    pub fn save_settings(&self, ui: &AppWindow) {
        self.edit_settings(ui);
        self.saver.retry();
    }
    pub fn reset_settings(&self, ui: &AppWindow) {
        self.settings.borrow_mut().reset();
        self.publish_settings(ui);
        ui.global::<UiState>().set_font_error("".into());
        ui.global::<UiState>().set_size_error("".into());
    }
    pub(in crate::application) fn persist_live(
        &self,
        ui: &AppWindow,
        updated: settings::ClientSettings,
    ) -> bool {
        let previous = self.settings.borrow().saved.clone();
        if previous != updated {
            if previous.avatar.visible != updated.avatar.visible {
                let view = ui.global::<UiState>();
                view.set_avatar_ready(false);
                view.set_avatar_status(
                    if updated.avatar.visible {
                        "キャラクターを読み込んでいます…"
                    } else {
                        "キャラクターは非表示です。"
                    }
                    .into(),
                );
            }
            if previous.avatar != updated.avatar
                && let Some(avatar) = &self.avatar
            {
                avatar.configure(updated.avatar.clone());
            }
            self.saver.update(updated.clone());
            ui.global::<UiState>().set_save_status("保存待ち…".into());
        }
        self.settings.borrow_mut().accept_live(updated);
        self.publish_settings(ui);
        true
    }
    pub fn selection_result(
        &self,
        ui: &AppWindow,
        generation: u64,
        motion: bool,
        result: Result<Option<String>, String>,
    ) {
        ui.global::<UiState>().set_picker_active(false);
        let result = self
            .settings
            .borrow_mut()
            .selection_result(generation, motion, result);
        match result {
            Ok(true) => self.apply_draft(ui),
            Ok(false) => {}
            Err(error) => {
                ui.global::<UiState>().set_apply_error(error.into());
                ui.global::<UiState>().set_error_field("".into());
            }
        }
    }
}

pub(super) fn project(d: &settings::Draft) -> SettingsDraft {
    SettingsDraft {
        font_family: d.font_family.clone().into(),
        text_size: d.text_size,
        theme_index: d.theme_index,
        model: d.model.clone().into(),
        motion: d.motion.clone().into(),
        motion_playing: d.motion_playing,
        motion_loop: d.motion_loop,
        fps_index: d.fps_index,
        size_text: d.size_text.clone().into(),
    }
}
pub(super) fn read_draft(d: SettingsDraft) -> settings::Draft {
    settings::Draft {
        font_family: d.font_family.to_string(),
        text_size: d.text_size,
        theme_index: d.theme_index,
        model: d.model.to_string(),
        motion: d.motion.to_string(),
        motion_playing: d.motion_playing,
        motion_loop: d.motion_loop,
        fps_index: d.fps_index,
        size_text: d.size_text.to_string(),
    }
}
