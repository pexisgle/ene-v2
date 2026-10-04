use super::*;

impl Application {
    pub(super) fn publish_settings(&self, ui: &AppWindow) {
        let state = self.settings.borrow();
        let view = ui.global::<UiState>();
        view.set_font_family(state.saved.font_family.clone().into());
        view.set_text_size(state.saved.text_size);
        view.set_avatar_visible(state.saved.avatar.visible);
        view.set_motion_playing(state.saved.avatar.motion_playing);
        view.set_motion_loop(state.saved.avatar.motion_loop);
        view.set_motion_configured(state.saved.avatar.motion.is_some());
        view.set_motion_saved(state.saved.avatar.motion.clone().unwrap_or_default().into());
        view.set_settings_draft(project(&state.draft));
        view.set_settings_dirty(state.dirty());
        ui.global::<Theme>().invoke_apply(state.saved.theme_index);
        if let Some(tray) = &self.tray {
            tray.set_avatar_visible(state.saved.avatar.visible);
        }
    }
    pub fn edit_settings(&self, ui: &AppWindow) {
        let draft = read_draft(ui.global::<UiState>().get_settings_draft());
        let mut state = self.settings.borrow_mut();
        state.edit(draft);
        ui.global::<UiState>().set_settings_dirty(state.dirty());
        ui.global::<UiState>().set_save_status("".into());
    }
    pub fn save_settings(&self, ui: &AppWindow) {
        self.edit_settings(ui);
        let result = self.settings.borrow_mut().save_draft(&self.path);
        match result {
            Ok(()) => {
                self.publish_settings(ui);
                if let Some(avatar) = &self.avatar {
                    avatar.configure(self.settings.borrow().saved.avatar.clone());
                }
                let view = ui.global::<UiState>();
                view.set_apply_error("".into());
                view.set_error_field("".into());
                view.set_save_status("保存しました。".into());
            }
            Err(error) => {
                let view = ui.global::<UiState>();
                view.set_apply_error(error.message.into());
                view.set_error_field(error.field.into());
                view.set_save_status("".into());
                view.set_category(0);
                ui.invoke_focus_error();
            }
        }
    }
    pub fn reset_settings(&self, ui: &AppWindow) {
        self.settings.borrow_mut().reset();
        self.publish_settings(ui);
        let view = ui.global::<UiState>();
        view.set_apply_error("".into());
        view.set_error_field("".into());
        view.set_save_status("変更を戻しました。".into());
    }
    pub(super) fn persist_live(&self, ui: &AppWindow, updated: settings::ClientSettings) -> bool {
        if let Err(error) = updated.save(&self.path) {
            ui.global::<UiState>().set_apply_error(error.into());
            ui.global::<UiState>().set_error_field("".into());
            return false;
        }
        self.settings.borrow_mut().accept_live(updated);
        self.publish_settings(ui);
        if let Some(avatar) = &self.avatar {
            avatar.configure(self.settings.borrow().saved.avatar.clone());
        }
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
            Ok(true) => self.publish_settings(ui),
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
