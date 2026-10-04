use super::*;

pub(super) fn apply_theme(ui: &ChatWindow, settings: &SettingsWindow, index: i32) {
    ui.global::<Theme>().invoke_apply(index);
    settings.global::<Theme>().invoke_apply(index);
    ui.set_theme_index(index);
    settings.set_theme_index(index);
}

pub(crate) fn bind_settings(ui: &ChatWindow, settings: &SettingsWindow) {
    // Prepare the hidden window at startup, including its native adapter,
    // instead of relying on an earlier visit from the chat window.
    settings
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    let weak = settings.as_weak();
    let chat_weak = ui.as_weak();
    ui.on_open_settings(move || {
        if let (Some(settings), Some(ui)) = (weak.upgrade(), chat_weak.upgrade()) {
            show_settings(&ui, &settings);
        }
    });
    let weak = settings.as_weak();
    let chat_weak = ui.as_weak();
    settings.on_apply_settings(move || {
        if let (Some(settings), Some(ui)) = (weak.upgrade(), chat_weak.upgrade()) {
            if !settings.invoke_commit_avatar_settings() {
                return;
            }
            apply_theme(&ui, &settings, settings.get_theme_index());
            let font = settings.get_font_family();
            if !font.trim().is_empty() {
                ui.set_font_family(font);
            }
            ui.set_text_size(settings.get_text_size().clamp(12, 28));
            show_chat(&ui);
            let _ = settings.hide();
        }
    });
}

pub(super) fn reflect_settings(
    ui: &ChatWindow,
    settings: &SettingsWindow,
    tray: Option<&tray::Ui>,
    value: &avatar_service::ClientSettings,
) {
    apply_theme(ui, settings, value.theme_index);
    ui.set_font_family(value.font_family.clone().into());
    ui.set_text_size(value.text_size);
    ui.set_avatar_size_percent(value.avatar.size_percent as i32);
    ui.set_avatar_model(value.avatar.model.clone().into());
    ui.set_avatar_motion(value.avatar.motion.clone().unwrap_or_default().into());
    ui.set_motion_playing(value.avatar.motion_playing);
    ui.set_motion_loop(value.avatar.motion_loop);
    ui.set_avatar_visible(value.avatar.visible);
    ui.set_avatar_fps_index(i32::from(value.avatar.fps == 60));
    settings.set_theme_index(value.theme_index);
    settings.set_font_family(ui.get_font_family());
    settings.set_text_size(value.text_size);
    settings.set_avatar_size_percent(ui.get_avatar_size_percent());
    settings.set_avatar_size_text(value.avatar.size_percent.to_string().into());
    settings.set_avatar_model(ui.get_avatar_model());
    settings.set_avatar_motion(ui.get_avatar_motion());
    settings.set_motion_playing(value.avatar.motion_playing);
    settings.set_motion_loop(value.avatar.motion_loop);
    settings.set_avatar_visible(value.avatar.visible);
    settings.set_avatar_fps_index(ui.get_avatar_fps_index());
    if let Some(tray) = tray {
        tray.set_avatar_visible(value.avatar.visible);
    }
}
pub(super) fn save_settings(
    updated: avatar_service::ClientSettings,
    saved: &RefCell<avatar_service::ClientSettings>,
    ui: &ChatWindow,
    settings: &SettingsWindow,
    tray: Option<&tray::Ui>,
    controller: &avatar_service::Controller,
    path: &std::path::Path,
) -> bool {
    if let Err(error) = updated.save(path) {
        settings.set_apply_error(error.clone().into());
        ui.set_avatar_status(error.into());
        return false;
    }
    reflect_settings(ui, settings, tray, &updated);
    controller.configure(updated.avatar.clone());
    *saved.borrow_mut() = updated;
    true
}
