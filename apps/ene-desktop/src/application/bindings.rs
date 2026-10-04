use super::*;

impl Application {
    pub(super) fn bind_actions(self: &Rc<Self>, ui: &AppWindow) {
        macro_rules! bind {
            ($on:ident, $method:ident) => {{
                let weak = Rc::downgrade(self);
                let view = ui.as_weak();
                ui.global::<UiState>().$on(move || {
                    if let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) {
                        app.$method(&ui);
                    }
                });
            }};
        }
        bind!(on_settings_edited, edit_settings);
        bind!(on_save_settings, save_settings);
        bind!(on_reset_settings, reset_settings);
        bind!(on_submit, submit);
        bind!(on_toggle_avatar, toggle_avatar);
        bind!(on_recenter_avatar, recenter_avatar);
        bind!(on_restart_avatar, restart_avatar);
        bind!(on_request_quit, request_quit);
        bind!(on_start_demo, start_demo);
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>()
            .on_set_motion_playing(move |playing| {
                if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                    let mut updated = app.settings.borrow().saved.clone();
                    updated.avatar.motion_playing = playing;
                    app.persist_live(&ui, updated);
                }
            });
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_set_motion_loop(move |repeat| {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                let mut updated = app.settings.borrow().saved.clone();
                updated.avatar.motion_loop = repeat;
                app.persist_live(&ui, updated);
            }
        });
        let view = ui.as_weak();
        ui.global::<UiState>().on_show_host_selection(move || {
            if let Some(ui) = view.upgrade() {
                show_host_selection(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.global::<UiState>().on_navigate(move |page| {
            if let Some(ui) = weak.upgrade() {
                navigate(&ui, page);
            }
        });
        let view = ui.as_weak();
        ui.global::<UiState>()
            .on_reveal_settings_item(move |y, height| {
                if let Some(ui) = view.upgrade() {
                    ui.invoke_reveal_settings_item(y, height);
                }
            });
        let mode = self.mode;
        let weak = ui.as_weak();
        ui.global::<UiState>().on_draft_edited(move |text| {
            if let Some(ui) = weak.upgrade() {
                ui.global::<UiState>().set_can_send(mode.accepts(&text));
            }
        });
        ui.global::<UiState>().on_discard_and_quit(quit);
        let weak = ui.as_weak();
        ui.global::<UiState>().on_cancel_quit(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<UiState>().set_quit_confirmation(false);
                focus_page(&ui);
            }
        });
        let weak = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.on_avatar_events_available(move || {
            if let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) {
                app.drain_avatar(&ui);
            }
        });
        let weak = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.window().on_close_requested(move || {
            let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) else {
                return slint::CloseRequestResponse::KeepWindowShown;
            };
            if app.tray_ready() {
                slint::CloseRequestResponse::HideWindow
            } else {
                app.request_quit(&ui);
                slint::CloseRequestResponse::KeepWindowShown
            }
        });
        if let Some(tray) = &self.tray {
            let view = ui.as_weak();
            tray.on_show_chat(move || {
                if let Some(ui) = view.upgrade() {
                    navigate(&ui, 0);
                }
            });
            let view = ui.as_weak();
            tray.on_open_settings(move || {
                let view = view.clone();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ui) = view.upgrade() {
                        navigate(&ui, 1);
                    }
                });
            });
            let view = ui.as_weak();
            tray.on_open_about(move || {
                let view = view.clone();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ui) = view.upgrade() {
                        if !ui.global::<UiState>().get_quit_confirmation() {
                            ui.global::<UiState>().set_category(5);
                        }
                        navigate(&ui, 1);
                    }
                });
            });
            macro_rules! tray_bind {
                ($on:ident, $method:ident) => {{
                    let weak = Rc::downgrade(self);
                    let view = ui.as_weak();
                    tray.$on(move || {
                        if let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) {
                            app.$method(&ui);
                        }
                    });
                }};
            }
            tray_bind!(on_toggle_avatar, toggle_avatar);
            tray_bind!(on_recenter_avatar, recenter_avatar);
            tray_bind!(on_restart_avatar, restart_avatar);
            tray_bind!(on_quit, request_quit);
        }
    }
}
