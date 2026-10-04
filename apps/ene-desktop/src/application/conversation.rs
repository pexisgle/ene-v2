use super::*;

impl Application {
    pub(super) fn start_demo(&self, ui: &AppWindow) {
        if self.mode != Mode::Demo || ui.global::<UiState>().get_quit_confirmation() {
            return;
        }
        ui.global::<UiState>().set_session_active(true);
        navigate(ui, 0);
    }
    pub(super) fn submit(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        if view.get_page() != 0
            || view.get_host_selection_visible()
            || !view.get_session_active()
            || view.get_quit_confirmation()
        {
            return;
        }
        let draft = view.get_draft();
        let Some(response) = self.mode.respond(&draft) else {
            return;
        };
        self.messages.push(ChatEntry {
            author: "あなた".into(),
            body: draft,
            provisional: false,
        });
        self.messages.push(ChatEntry {
            author: "ene".into(),
            body: response.into(),
            provisional: true,
        });
        view.set_draft("".into());
        view.set_can_send(false);
        ui.invoke_focus_input();
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_scroll_to_latest();
            }
        });
    }
}
