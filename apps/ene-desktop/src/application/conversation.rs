use super::*;

impl Application {
    pub(in crate::application) fn start_demo(&self, ui: &AppWindow) {
        if self.mode.get() != Mode::Demo || ui.global::<UiState>().get_quit_confirmation() {
            return;
        }
        ui.global::<UiState>().set_session_active(true);
        navigate(ui, 0);
    }
    pub(in crate::application) fn submit(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        if view.get_page() != 0
            || view.get_host_selection_visible()
            || !view.get_session_active()
            || view.get_quit_confirmation()
        {
            return;
        }
        let draft = view.get_draft();
        if self.mode.get() != Mode::Demo {
            self.refresh_send(ui);
            if !view.get_can_send() {
                return;
            }
            if let Some(host) = self.host.borrow().as_ref() {
                view.set_send_pending(true);
                if !host.request(
                    view.get_selected_companion_id().as_str(),
                    crate::infrastructure::host::Command::Send(draft.into()),
                ) {
                    view.set_send_pending(false);
                    view.set_conversation_error(
                        "接続処理が停止しました。ホスト画面から接続し直してください。".into(),
                    );
                }
                self.refresh_send(ui);
            }
            return;
        }
        let Some(response) = self.mode.get().respond(&draft) else {
            return;
        };
        self.messages.push(ChatEntry {
            author: "あなた".into(),
            body: draft,
            provisional: false,
            state: 0,
            outgoing: true,
        });
        self.messages.push(ChatEntry {
            author: "ene".into(),
            body: response.into(),
            provisional: true,
            state: 0,
            outgoing: false,
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
    pub(in crate::application) fn leave_demo(self: &Rc<Self>, ui: &AppWindow) {
        if self.mode.get() != Mode::Demo || ui.global::<UiState>().get_quit_confirmation() {
            return;
        }
        self.mode.set(Mode::Disconnected);
        self.start_monitor(ui);
        let state = ui.global::<UiState>();
        state.set_demo_mode(false);
        state.set_session_active(false);
        state.set_can_send(false);
        self.messages.set_vec(vec![]);
        state.set_status("このPCのホストに接続して会話を始めます。".into());
        if let Some(tray) = &self.tray {
            tray.set_connection_status("ホスト未接続");
        }
        navigate(ui, 0);
    }
    pub(in crate::application) fn choose_companion(&self, ui: &AppWindow, index: i32) {
        let state = ui.global::<UiState>();
        let Some(snapshot) = self.snapshot.borrow().clone() else {
            return;
        };
        let old_id = state.get_selected_companion_id();
        let old_index = snapshot
            .companions
            .iter()
            .position(|c| c.id == old_id.as_str());
        state.set_companion_index(old_index.map_or(-1, |i| i as i32));
        if !state.get_can_switch_companion() {
            return;
        }
        let Some(companion) = usize::try_from(index)
            .ok()
            .and_then(|i| snapshot.companions.get(i))
        else {
            return;
        };
        if companion.id == old_id.as_str() {
            return;
        }
        self.conversation_drafts
            .borrow_mut()
            .insert(old_id.to_string(), state.get_draft().to_string());
        let draft = self
            .conversation_drafts
            .borrow_mut()
            .remove(&companion.id)
            .unwrap_or_default();
        state.set_draft(draft.into());
        state.set_selected_companion_id(companion.id.clone().into());
        state.set_provider_id("".into());
        state.set_model_id("".into());
        state.set_model_ids(Rc::new(VecModel::<slint::SharedString>::default()).into());
        state.set_host_feedback("".into());
        state.set_conversation_error("".into());
        state.set_connection_matches_provider(false);
        let messages = self.history.borrow().clone();
        self.handle_host_event(
            ui,
            crate::infrastructure::host::Event::View(Box::new(snapshot), messages),
        );
        self.refresh_send(ui);
        ui.invoke_scroll_to_latest();
    }
    pub(in crate::application) fn refresh_send(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        let ready = if self.mode.get() == Mode::Demo {
            true
        } else {
            view.get_host_connected() && view.get_chat_ready() && !view.get_send_pending()
        };
        view.set_can_send(ready && !view.get_draft().trim().is_empty());
    }
}
