use super::*;

impl Application {
    pub fn request_quit(&self, ui: &AppWindow) {
        // Capture any edit even if the event-loop's edit notification is pending.
        self.edit_settings(ui);
        if self.settings.borrow().dirty()
            || ui.global::<UiState>().get_host_dirty()
            || ui.global::<UiState>().get_host_busy()
            || !ui.global::<UiState>().get_api_key().is_empty()
            || ui.global::<UiState>().get_send_pending()
            || !ui.global::<UiState>().get_draft().trim().is_empty()
            || self
                .conversation_drafts
                .borrow()
                .values()
                .any(|text| !text.trim().is_empty())
        {
            ui.global::<UiState>().set_quit_confirmation(true);
            show(ui);
            ui.invoke_focus_quit();
        } else {
            quit();
        }
    }
    pub(in crate::application) fn tray_ready(&self) -> bool {
        self.registration
            .borrow()
            .as_ref()
            .is_some_and(|r| r.usable_at(Instant::now()))
    }
    pub fn start_services(
        self: &Rc<Self>,
        ui: &AppWindow,
        receiver: Receiver<tray::Registration>,
    ) -> slint::Timer {
        let monitor = slint::Timer::default();
        let weak = Rc::downgrade(self);
        let view = ui.as_weak();
        monitor.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                if let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) {
                    for update in receiver.try_iter() {
                        *app.registration.borrow_mut() = Some(update);
                    }
                    let ready = app.tray_ready();
                    let state = ui.global::<UiState>();
                    state.set_tray_ready(ready);
                    state.set_tray_status(
                        if ready {
                            "×は非表示 · トレイから再表示できます。"
                        } else {
                            "トレイなし · ×でクライアントを終了します。"
                        }
                        .into(),
                    );
                    if !ready && !ui.window().is_visible() {
                        show(&ui);
                    }
                }
            },
        );
        let view = ui.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = view.upgrade() {
                ui.invoke_avatar_events_available();
                ui.invoke_host_events_available();
            }
        });
        if let Some(avatar) = &self.avatar {
            avatar.placement(self.settings.borrow().saved.placement.clone());
            avatar.configure(self.settings.borrow().saved.avatar.clone());
        }
        ui.global::<UiState>()
            .set_avatar_status("キャラクターを読み込んでいます…".into());
        monitor
    }
}
pub(super) fn quit() {
    if let Err(error) = slint::quit_event_loop() {
        eprintln!("クライアントを終了できません: {error}");
    }
}
