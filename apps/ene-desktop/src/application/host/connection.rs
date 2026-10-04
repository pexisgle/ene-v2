use super::super::*;

impl Application {
    pub(in crate::application) fn start_host(self: &Rc<Self>, ui: &AppWindow) {
        if self.mode.get() == Mode::Demo || ui.global::<UiState>().get_quit_confirmation() {
            return;
        }
        if ui.global::<UiState>().get_host_connected() {
            navigate(ui, 0);
            return;
        }
        if self.host.borrow().is_none() {
            let view = ui.as_weak();
            *self.host.borrow_mut() =
                Some(crate::infrastructure::host::Controller::new(move || {
                    let _ = view.upgrade_in_event_loop(|ui| ui.invoke_host_events_available());
                }));
        }
        ui.global::<UiState>().set_connection_phase(1);
        ui.global::<UiState>().set_connection_failed(false);
        ui.global::<UiState>()
            .set_connection_detail("ホストを確認しています…".into());
        ui.global::<UiState>()
            .set_status("このPCのホストに接続しています…".into());
    }
    pub(in crate::application) fn reconnect_host(self: &Rc<Self>, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        if state.get_connection_pending() || state.get_host_connected() {
            return;
        }
        if state.get_connection_phase() == 0 {
            self.host.borrow_mut().take();
            self.start_host(ui);
        } else if let Some(host) = self.host.borrow().as_ref() {
            // Wake the existing retry loop: keep any uncertain send's idempotency key.
            if host.request(
                state.get_selected_companion_id().as_str(),
                crate::infrastructure::host::Command::Reconnect,
            ) {
                state.set_connection_phase(1);
                state.set_connection_detail("接続を確認しています…".into());
                state.set_status("接続を確認しています…".into());
            }
        }
    }
    pub(in crate::application) fn disconnect_host(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        if state.get_send_pending() || state.get_host_busy() {
            return;
        }
        self.host.borrow_mut().take();
        state.set_host_connected(false);
        state.set_connection_phase(0);
        state.set_connection_failed(false);
        state.set_retry_seconds(0);
        state.set_chat_ready(false);
        state.set_api_key("".into());
        let detail = "このアプリの接続を解除しました。ホストの起動・停止状態は別に確認できます。";
        state.set_connection_detail(detail.into());
        state.set_status(detail.into());
        if let Some(tray) = &self.tray {
            tray.set_connection_status("ホスト未接続");
        }
        self.refresh_send(ui);
    }
    pub(in crate::application) fn start_monitor(&self, ui: &AppWindow) {
        if self.monitor.borrow().is_none() {
            let view = ui.as_weak();
            *self.monitor.borrow_mut() = Some(crate::infrastructure::host::Controller::monitor(
                move || {
                    let _ = view.upgrade_in_event_loop(|ui| ui.invoke_host_events_available());
                },
            ));
        }
    }
    pub(in crate::application) fn inspect_host(&self, ui: &AppWindow) {
        if self.mode.get() == Mode::Demo {
            return;
        }
        self.start_monitor(ui);
        let accepted = self.monitor.borrow().as_ref().is_some_and(|monitor| {
            monitor.request("", crate::infrastructure::host::Command::InspectHost)
        });
        if !accepted {
            self.monitor.borrow_mut().take();
            self.start_monitor(ui);
        }
        ui.global::<UiState>().set_runtime_checking(true);
    }
}
