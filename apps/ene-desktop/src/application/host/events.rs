use super::super::*;

impl Application {
    pub(in crate::application) fn drain_host(&self, ui: &AppWindow) {
        let observed: Vec<_> = self
            .monitor
            .borrow()
            .as_ref()
            .map(|host| host.events.try_iter().collect())
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
    fn handle_action(
        &self,
        ui: &AppWindow,
        result: crate::infrastructure::host::ActionResultEnvelope,
    ) {
        use crate::infrastructure::host::ActionEvent;
        let state = ui.global::<UiState>();
        let Some(reduction) = self
            .host_ui
            .borrow_mut()
            .reduce(&result.context, &result.payload)
        else {
            return;
        };
        if reduction.clear_draft == Some(crate::host_model::DraftResource::Connection) {
            if let Some(host) = self.host.borrow().as_ref() {
                host.saved(&result.context);
            }
            state.set_api_key("".into());
            state.set_credential_edit(0);
        }
        if let Some(id) = reduction.select_connection {
            state.set_selected_connection_id(id.into());
        }
        match result.payload {
            ActionEvent::Models(id, list)
                if reduction.same_edit
                    && id == state.get_selected_connection_id().as_str()
                    && state.get_connection_matches_provider() =>
            {
                state.set_model_ids(
                    Rc::new(VecModel::from(
                        list.iter()
                            .map(|model| model.id.clone().into())
                            .collect::<Vec<slint::SharedString>>(),
                    ))
                    .into(),
                );
                self.models.replace(list);
                self.project_model_info(ui);
            }
            ActionEvent::Older(page) => {
                if !page.items.is_empty() {
                    ui.invoke_prepare_history_prepend();
                }
                state.set_has_older(page.next_cursor.is_some());
            }
            ActionEvent::Availability(value) => {
                let pending = self
                    .availability_request
                    .replace(super::AvailabilityRequest::Idle);
                let (id, signature) = match pending {
                    super::AvailabilityRequest::Pending { id, signature } => (id, signature),
                    other => {
                        self.availability_request.replace(other);
                        return;
                    }
                };
                if id != result.context.id {
                    self.availability_request
                        .replace(super::AvailabilityRequest::Pending { id, signature });
                    return;
                }
                let current =
                    self.snapshot.borrow().as_ref().is_some_and(|snapshot| {
                        snapshot.settings.revision == value.settings_revision
                    });
                let provider_current = value.provider_revision.as_ref().is_some_and(|revision| {
                    self.providers.borrow().iter().any(|provider| {
                        provider.id == state.get_provider_id().as_str()
                            && &provider.revision == revision
                    })
                });
                if current && provider_current {
                    self.availability_request
                        .replace(super::AvailabilityRequest::Ready(signature));
                    state.set_availability_state(2);
                    self.availability.replace(Some(value));
                    self.project_permission_scope(ui);
                    self.project_permission_flags(ui);
                } else {
                    self.availability_request
                        .replace(super::AvailabilityRequest::Failed(signature));
                    state.set_availability_state(3);
                }
            }
            ActionEvent::Error(_)
                if matches!(
                    result.context.target,
                    crate::infrastructure::host::OperationTarget::Availability(_)
                ) =>
            {
                let pending = self
                    .availability_request
                    .replace(super::AvailabilityRequest::Idle);
                if let super::AvailabilityRequest::Pending { id, signature } = &pending
                    && *id == result.context.id
                {
                    self.availability_request
                        .replace(super::AvailabilityRequest::Failed(signature.clone()));
                    state.set_availability_state(3);
                } else {
                    self.availability_request.replace(pending);
                }
            }
            ActionEvent::Error(error)
                if reduction.foreground
                    && result.context.target
                        == crate::infrastructure::host::OperationTarget::History =>
            {
                state.set_conversation_error(error.into())
            }
            ActionEvent::Error(_)
            | ActionEvent::ConnectionSaved(_)
            | ActionEvent::Done(..)
            | ActionEvent::Models(..) => {}
        }
        self.project_host_model(ui);
    }
    pub(crate) fn handle_host_event(
        &self,
        ui: &AppWindow,
        event: crate::infrastructure::host::Event,
    ) {
        use crate::infrastructure::host::{ConnectionPhase, Event};
        let state = ui.global::<UiState>();
        match event {
            Event::Action(result) => self.handle_action(ui, *result),
            Event::Session(identity) => {
                self.host_ui.borrow_mut().set_session(identity);
                self.availability_request
                    .replace(super::AvailabilityRequest::Idle);
                state.set_availability_state(0);
                self.availability.replace(None);
                self.project_host_model(ui);
            }
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
                    .position(|provider| provider.id == current.as_str())
                    .unwrap_or(0) as i32;
                state.set_provider_names(
                    Rc::new(VecModel::from(
                        providers
                            .iter()
                            .map(|provider| provider.name.clone().into())
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
                self.refresh_availability(ui);
            }
            Event::View(snapshot, messages) => {
                if !self.project_snapshot(ui, *snapshot, messages) {
                    return;
                }
            }
            Event::Connection(phase, text) => {
                let connected = phase == ConnectionPhase::Connected;
                state.set_connection_phase(phase as i32);
                state.set_host_connected(connected);
                state.set_connection_failed(phase == ConnectionPhase::Stopped);
                state.set_connection_detail(text.clone().into());
                state.set_status(text.into());
                if phase != ConnectionPhase::Retrying {
                    state.set_retry_seconds(0);
                }
                if connected {
                    state.set_runtime_state(2);
                    state.set_runtime_detail("".into());
                    state.set_runtime_age(0);
                    self.refresh_availability(ui);
                } else {
                    self.availability_request
                        .replace(super::AvailabilityRequest::Idle);
                    self.availability.replace(None);
                    state.set_availability_state(0);
                    let mut model = self.host_ui.borrow_mut();
                    model.session = None;
                    if model.busy.take().is_some() {
                        model.feedback_error = true;
                        model.feedback = "接続が切れたため結果を確認できません。再接続後に保存内容を確認してください。".into();
                    }
                    drop(model);
                    self.project_host_model(ui);
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
            Event::Error(text) => {
                let mut model = self.host_ui.borrow_mut();
                model.busy = None;
                model.feedback_error = true;
                model.feedback = text;
                drop(model);
                self.project_host_model(ui);
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
