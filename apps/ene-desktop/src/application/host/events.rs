use super::super::*;

impl Application {
    pub(in crate::application) fn drain_host(&self, ui: &AppWindow) {
        let observed: Vec<_> = self
            .monitor
            .borrow()
            .as_ref()
            .map(|monitor| monitor.events.try_iter().collect())
            .unwrap_or_default();
        for event in observed {
            self.handle_host_event(ui, event);
        }
        let events: Vec<_> = self
            .host
            .borrow()
            .as_ref()
            .map(|host| host.events.try_iter().chain(host.take_view()).collect())
            .unwrap_or_default();
        for event in events {
            self.handle_host_event(ui, event);
        }
    }
    pub(crate) fn handle_host_event(
        &self,
        ui: &AppWindow,
        event: crate::infrastructure::host::Event,
    ) {
        use crate::infrastructure::host::Event;
        let state = ui.global::<UiState>();
        let (event, completed) = match event {
            Event::Action(request, event) => {
                let host = self.host.borrow();
                if host.as_ref().is_none_or(|host| !host.is_current(&request))
                    || request.companion != state.get_selected_companion_id().as_str()
                {
                    return;
                }
                if matches!(event.as_ref(), Event::Done(_, true, _))
                    && request.id == state.get_host_request_id().as_str()
                    && request.edit_revision == state.get_host_edit_revision()
                {
                    host.as_ref().expect("current host").saved(&request);
                }
                if request.id != state.get_host_request_id().as_str() {
                    if matches!(event.as_ref(), Event::Done(_, true, _)) {
                        mark_host_saved(ui, &request);
                    }
                    return;
                }
                (*event, Some(request))
            }
            event => (event, None),
        };
        match event {
            Event::Action(..) => unreachable!("nested action event"),
            Event::Runtime(observed) => {
                state.set_runtime_state(observed.state);
                state.set_runtime_endpoint(observed.endpoint.into());
                state.set_runtime_detail(observed.detail.into());
                state.set_runtime_checking(false);
                state.set_runtime_age(0);
            }
            Event::RetryIn(seconds) => state.set_retry_seconds(seconds),
            Event::Providers(list) => {
                let current = state.get_provider_id();
                self.providers.replace(list);
                let providers = self.providers.borrow();
                let index = providers
                    .iter()
                    .position(|p| p.id == current.as_str())
                    .unwrap_or(0) as i32;
                state.set_provider_names(
                    Rc::new(VecModel::from(
                        providers
                            .iter()
                            .map(|p| p.name.clone().into())
                            .collect::<Vec<slint::SharedString>>(),
                    ))
                    .into(),
                );
                drop(providers);
                self.project_provider(
                    ui,
                    index,
                    !state.get_connection_dirty() && current.is_empty(),
                );
                self.project_permission_scope(ui);
            }
            Event::Models(id, list) => {
                state.set_host_operation(0);
                if id != state.get_selected_connection_id().as_str()
                    || !state.get_connection_matches_provider()
                {
                    return;
                }
                state.set_model_ids(
                    Rc::new(VecModel::from(
                        list.iter()
                            .map(|m| m.id.clone().into())
                            .collect::<Vec<slint::SharedString>>(),
                    ))
                    .into(),
                );
                self.models.replace(list);
                self.project_model_info(ui);
                state.set_host_feedback("モデル一覧・情報を取得しました".into());
            }
            Event::View(snapshot, messages) => {
                if !self.project_snapshot(ui, *snapshot, messages) {
                    return;
                }
            }
            Event::Connection(phase, text) => {
                let connected = phase == crate::infrastructure::host::ConnectionPhase::Connected;
                state.set_connection_phase(phase as i32);
                state.set_host_connected(connected);
                state.set_connection_failed(
                    phase == crate::infrastructure::host::ConnectionPhase::Stopped,
                );
                state.set_connection_detail(text.clone().into());
                state.set_status(text.into());
                if phase != crate::infrastructure::host::ConnectionPhase::Retrying {
                    state.set_retry_seconds(0);
                }
                if connected {
                    state.set_runtime_state(2);
                    state.set_runtime_detail("".into());
                    state.set_runtime_age(0);
                }
                if !connected && state.get_host_busy() {
                    state.set_host_operation(0);
                    state.set_host_feedback_error(true);
                    state.set_host_feedback("接続が切れたため結果を確認できません。再接続後に保存内容を確認してください。".into());
                }
                if connected && !state.get_session_active() {
                    state.set_session_active(true);
                    if state.get_host_selection_visible() {
                        if state.get_chat_ready() {
                            navigate(ui, 0);
                        } else {
                            state.set_category(state.get_setup_category());
                            navigate(ui, 1);
                        }
                    }
                }
            }
            Event::Accepted(text) => {
                state.set_conversation_error("".into());
                state.set_send_pending(false);
                if state.get_draft() == text {
                    state.set_draft("".into());
                }
                if state.get_page() == 0
                    && !state.get_host_selection_visible()
                    && !state.get_quit_confirmation()
                {
                    ui.invoke_focus_input();
                }
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.invoke_scroll_to_latest();
                    }
                });
            }
            Event::SendFailed(text) => {
                state.set_conversation_error(text.clone().into());
                state.set_send_pending(false);
                state.set_status(text.into());
            }
            Event::ConnectionSaved(id) => {
                if completed
                    .as_ref()
                    .is_some_and(|r| r.edit_revision == state.get_host_edit_revision())
                {
                    state.set_selected_connection_id(id.into());
                }
            }
            Event::Error(text) => {
                if state.get_host_operation() == 4 {
                    state.set_conversation_error(text.clone().into());
                }
                state.set_host_operation(0);
                state.set_host_feedback_error(true);
                state.set_host_feedback(text.into());
            }
            Event::Done(text, committed, connection) => {
                state.set_host_feedback_error(false);
                if committed && let Some(request) = completed {
                    mark_host_saved(ui, &request);
                    if request.edit_revision == state.get_host_edit_revision()
                        && let Some(id) = connection
                    {
                        state.set_selected_connection_id(id.into());
                    }
                }
                state.set_host_feedback(text.into());
                state.set_host_operation(0);
            }
            Event::Older(page) => {
                if !page.items.is_empty() {
                    ui.invoke_prepare_history_prepend();
                }
                state.set_has_older(page.next_cursor.is_some());
                state.set_host_operation(0);
                state.set_host_feedback("過去の会話を読み込みました。".into());
            }
        }
        self.refresh_send(ui);
        if let Some(tray) = &self.tray {
            tray.set_connection_status(if state.get_host_connected() {
                if state.get_chat_ready() {
                    "会話できます"
                } else {
                    "AI設定が必要です"
                }
            } else if state.get_connection_phase() == 3 {
                "ホストに再接続しています"
            } else {
                "ホスト未接続"
            });
        }
    }
}

pub(crate) fn mark_host_saved(
    ui: &AppWindow,
    request: &crate::infrastructure::host::RequestContext,
) {
    let state = ui.global::<UiState>();
    if request.edit_revision != state.get_host_edit_revision() {
        return;
    }
    use crate::infrastructure::host::OperationTarget;
    match request.target {
        OperationTarget::Configuration(_) => {
            state.set_connection_dirty(false);
            state.set_api_key("".into());
            state.set_credential_edit(0);
        }
        OperationTarget::Companion => state.set_companion_dirty(false),
        OperationTarget::Permission(_) => state.set_permissions_dirty(false),
        _ => {}
    }
}
