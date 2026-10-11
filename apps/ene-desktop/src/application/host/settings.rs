use super::super::*;

impl Application {
    pub(in crate::application) fn project_host_model(&self, ui: &AppWindow) {
        use crate::host_model::DraftResource;
        let state = ui.global::<UiState>();
        let model = self.host_ui.borrow();
        state.set_connection_dirty(model.dirty(DraftResource::Connection));
        state.set_companion_dirty(model.dirty(DraftResource::Companion));
        state.set_permissions_dirty(model.dirty(DraftResource::Permission));
        state.set_host_operation(
            model
                .busy
                .as_ref()
                .map_or(0, |(_, feedback)| feedback.operation()),
        );
        state.set_host_feedback(model.feedback.clone().into());
        state.set_host_feedback_error(model.feedback_error);
    }
    pub(in crate::application) fn edit_host_resource(
        &self,
        ui: &AppWindow,
        resource: crate::host_model::DraftResource,
    ) {
        let companion = ui.global::<UiState>().get_selected_companion_id();
        self.host_ui.borrow_mut().edit(companion.as_str(), resource);
        self.project_host_model(ui);
    }
    pub(in crate::application) fn reset_host_resource(
        &self,
        ui: &AppWindow,
        resource: crate::host_model::DraftResource,
    ) {
        let companion = ui.global::<UiState>().get_selected_companion_id();
        self.host_ui
            .borrow_mut()
            .reset(companion.as_str(), resource);
        self.project_host_model(ui);
    }
    pub(in crate::application) fn project_permission_flags(&self, ui: &AppWindow) {
        if ui.global::<UiState>().get_permissions_dirty() {
            return;
        }
        let availability = self.availability.borrow();
        let granted = |operation: &str| {
            availability.as_ref().is_some_and(|value| {
                value
                    .operations
                    .iter()
                    .any(|item| item.operation == operation && item.authorization == "granted")
            })
        };
        ui.global::<UiState>()
            .set_allow_conversation(granted("conversation"));
        ui.global::<UiState>()
            .set_allow_check(granted("connection_check") && granted("model_list"));
    }
    pub(in crate::application) fn refresh_availability(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        let companion = state.get_selected_companion_id().to_string();
        let connection = state.get_selected_connection_id().to_string();
        let snapshot = self.snapshot.borrow();
        let Some(snapshot) = snapshot.as_ref() else {
            return;
        };
        let provider = self.providers.borrow();
        let signature = serde_json::json!({
            "host": snapshot.host_id, "epoch": snapshot.cursor.epoch, "connected": state.get_host_connected(),
            "companion": companion, "connection": connection, "settings": snapshot.settings.revision,
            "grants": snapshot.grants.iter().filter(|grant| grant.companion_id == companion && grant.connection_id == connection).collect::<Vec<_>>(),
            "provider": provider.iter().find(|provider| provider.id == state.get_provider_id().as_str()).map(|provider| (&provider.revision, &provider.state)),
            "health": snapshot.health,
        });
        drop(provider);
        if self.availability_request.borrow().signature() == Some(&signature) {
            self.project_permission_flags(ui);
            self.project_permission_scope(ui);
            return;
        }
        self.availability_request
            .replace(super::AvailabilityRequest::Idle);
        self.availability.replace(None);
        state.set_availability_state(0);
        self.project_permission_flags(ui);
        self.project_permission_scope(ui);
        if state.get_host_connected() && !connection.is_empty() && !companion.is_empty() {
            if let Some(id) = self.try_host_request(
                ui,
                crate::infrastructure::host::Command::Availability(connection),
            ) {
                self.availability_request
                    .replace(super::AvailabilityRequest::Pending { id, signature });
                state.set_availability_state(1);
            } else {
                self.availability_request
                    .replace(super::AvailabilityRequest::Failed(signature));
                state.set_availability_state(3);
            }
        }
    }
    pub(in crate::application) fn retry_availability(&self, ui: &AppWindow) {
        if matches!(
            *self.availability_request.borrow(),
            super::AvailabilityRequest::Pending { .. }
        ) {
            return;
        }
        self.availability_request
            .replace(super::AvailabilityRequest::Idle);
        self.refresh_availability(ui);
    }
    pub(in crate::application) fn project_permission_scope(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        // Preserve both the displayed destinations and their versions for the
        // whole consent draft, including provider reload notifications.
        if state.get_permissions_dirty() {
            return;
        }
        let availability = self.availability.borrow();
        let scope = availability
            .as_ref()
            .filter(|value| {
                value.connection_id == state.get_selected_connection_id().as_str()
                    && value.companion_id == state.get_selected_companion_id().as_str()
            })
            .and_then(|value| {
                Some(super::PermissionDraft {
                    connection: value.connection_id.clone(),
                    consent: ene_rpc::Consent {
                        settings_revision: value.settings_revision,
                        provider_revision: value.provider_revision.clone()?,
                        ..Default::default()
                    },
                    destinations: value.destinations.clone(),
                })
            });
        state.set_destinations(
            scope
                .as_ref()
                .map(|s| s.destinations.join(" → "))
                .unwrap_or_default()
                .into(),
        );
        self.host_drafts.borrow_mut().permission = scope;
    }
    pub(in crate::application) fn reset_host_page(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        if !state.get_host_connected() || state.get_host_busy() {
            return;
        }
        match state.get_category() {
            1 => self.reset_host_resource(ui, crate::host_model::DraftResource::Companion),
            2 => {
                if let Some(host) = self.host.borrow().as_ref() {
                    host.discard_connection_draft(state.get_selected_companion_id().as_str());
                }
                self.reset_host_resource(ui, crate::host_model::DraftResource::Connection);
                state.set_api_key("".into());
                state.set_credential_edit(0);
                state.set_provider_id("".into());
            }
            3 => {
                self.reset_host_resource(ui, crate::host_model::DraftResource::Permission);
                if self.availability.borrow().is_none() {
                    self.retry_availability(ui);
                }
            }
            _ => return,
        }
        let snapshot = self.snapshot.borrow().clone();
        let messages = self.history.borrow().clone();
        if let Some(snapshot) = snapshot {
            self.handle_host_event(
                ui,
                crate::infrastructure::host::Event::View(Box::new(snapshot), messages),
            );
        }
        state.set_host_save_category(state.get_category());

        state.set_host_feedback_error(false);
        state.set_host_feedback("元に戻しました。".into());
    }
    pub(in crate::application) fn host_request(
        &self,
        ui: &AppWindow,
        command: crate::infrastructure::host::Command,
    ) {
        self.try_host_request(ui, command);
    }
    fn try_host_request(
        &self,
        ui: &AppWindow,
        command: crate::infrastructure::host::Command,
    ) -> Option<String> {
        let state = ui.global::<UiState>();
        let revoke = matches!(
            &command,
            crate::infrastructure::host::Command::RevokeCredential(_)
                | crate::infrastructure::host::Command::RevokePermissions(_)
        );
        if !state.get_host_connected()
            || (state.get_host_busy()
                && !revoke
                && command.policy().feedback != crate::infrastructure::host::Feedback::Quiet)
        {
            return None;
        }
        if let crate::infrastructure::host::Command::Permission { connection, .. } = &command
            && self
                .host_drafts
                .borrow()
                .permission
                .as_ref()
                .is_none_or(|p| &p.connection != connection)
        {
            state.set_host_feedback_error(true);
            state.set_host_feedback(
                "接続設定が変わりました。元に戻して送信先を再確認してください。".into(),
            );
            return None;
        }
        let mut accepted = None;
        if let Some(host) = self.host.borrow().as_ref() {
            let policy = command.policy();
            let companion = state.get_selected_companion_id().to_string();
            let stamp = policy
                .resource
                .map(|resource| self.host_ui.borrow_mut().stamp(&companion, resource))
                .unwrap_or_default();
            if let Some(id) = host.request_at(&companion, command, stamp) {
                accepted = Some(id.clone());
                if let Some(context) = host.context(&id) {
                    let mut model = self.host_ui.borrow_mut();
                    model.set_session(context.session.clone());
                    model.begin(&context, policy.feedback);
                }
                if policy.feedback != crate::infrastructure::host::Feedback::Quiet {
                    state.set_host_request_id(id.into());
                    state.set_host_save_category(match policy.feedback {
                        crate::infrastructure::host::Feedback::Connection => 2,
                        crate::infrastructure::host::Feedback::Companion => 1,
                        crate::infrastructure::host::Feedback::Permission => 3,
                        _ => -1,
                    });
                }
            } else if policy.feedback != crate::infrastructure::host::Feedback::Quiet {
                let mut model = self.host_ui.borrow_mut();
                model.feedback_error = true;
                model.feedback =
                    "要求を受け付けられません。処理の完了後に再試行してください。".into();
            }
            self.project_host_model(ui);
        }
        accepted
    }
    pub(in crate::application) fn project_provider(&self, ui: &AppWindow, index: i32, reset: bool) {
        let providers = self.providers.borrow();
        let Some(p) = providers.get(index.max(0) as usize) else {
            ui.global::<UiState>().set_provider_ready(false);
            ui.global::<UiState>().set_provider_index(-1);
            return;
        };
        let state = ui.global::<UiState>();
        let changed = state.get_provider_id() != p.id;
        if changed || reset {
            self.models.replace(p.models.clone());
            state.set_model_info("モデル情報は未取得です".into());
        }
        state.set_provider_index(index);
        state.set_provider_id(p.id.clone().into());
        state.set_provider_name(p.name.clone().into());
        state.set_provider_plugin(p.kind == "plugin");
        state.set_provider_ready(
            p.state == "ready"
                && p.settings
                    .iter()
                    .all(|f| matches!(f.kind.as_str(), "text" | "number" | "boolean" | "select")),
        );
        state.set_provider_state(p.state.clone().into());
        state.set_provider_error(p.error.clone().unwrap_or_default().into());
        state.set_requires_credentials(p.requires_credentials);
        state.set_environment_variable(p.environment_variable.clone().unwrap_or_default().into());
        state.set_model_list_supported(p.capabilities.model_list && p.state == "ready");

        if changed
            || reset
            || (!p.settings.is_empty() && state.get_provider_fields().row_count() == 0)
        {
            if changed || reset || state.get_provider_endpoint().is_empty() {
                state.set_provider_endpoint(p.endpoint.clone().into());
            }
            if changed || reset || state.get_model_id().is_empty() {
                state.set_model_id(
                    p.models
                        .first()
                        .map(|m| m.id.clone())
                        .unwrap_or_default()
                        .into(),
                );
            }
            state.set_model_ids(
                Rc::new(VecModel::from(
                    p.models
                        .iter()
                        .map(|m| m.id.clone().into())
                        .collect::<Vec<slint::SharedString>>(),
                ))
                .into(),
            );
            state.set_provider_fields(
                Rc::new(VecModel::from(
                    p.settings
                        .iter()
                        .map(|f| {
                            let options: serde_json::Value =
                                serde_json::from_str(&p.options.json).unwrap_or_default();
                            let value = options.get(&f.key);
                            ProviderField {
                                key: f.key.clone().into(),
                                label: f.label.clone().into(),
                                kind: match f.kind.as_str() {
                                    "text" => 0,
                                    "number" => 1,
                                    "boolean" => 2,
                                    "select" => 3,
                                    _ => 4,
                                },
                                value: value
                                    .map(|v| {
                                        v.as_str()
                                            .map(str::to_owned)
                                            .unwrap_or_else(|| v.to_string())
                                    })
                                    .unwrap_or_default()
                                    .into(),
                                checked: value.and_then(|v| v.as_bool()).unwrap_or(false),
                                choices: Rc::new(VecModel::from(
                                    f.choices
                                        .iter()
                                        .map(|s| s.clone().into())
                                        .collect::<Vec<slint::SharedString>>(),
                                ))
                                .into(),
                            }
                        })
                        .collect::<Vec<_>>(),
                ))
                .into(),
            );
        }
        self.project_model_info(ui);
    }
    pub(in crate::application) fn project_model_info(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        // Keep a saved or manually entered ID selectable even when it is absent
        // from the provider catalog. ComboBox clears values outside its model.
        let mut choices: Vec<_> = state.get_model_ids().iter().collect();
        let selected = state.get_model_id();
        if !choices.is_empty() && !selected.is_empty() && !choices.contains(&selected) {
            choices.push(selected.clone());
        }
        let index = choices.iter().position(|id| id == &selected);
        if !state.get_model_choices().iter().eq(choices.iter().cloned()) {
            state.set_model_choices(Rc::new(VecModel::from(choices)).into());
        }
        state.set_model_choice_index(index.map_or(-1, |index| index as i32));
        let models = self.models.borrow();
        let info = models
            .iter()
            .find(|m| m.id == state.get_model_id().as_str())
            .and_then(|m| m.metadata.as_option());
        let text = if let Some(info) = info {
            let tokens = |v: Option<u64>| {
                v.map(|v| format!("{v} tokens"))
                    .unwrap_or_else(|| "不明".into())
            };
            let formats = |v: Option<&ene_rpc::StringList>| {
                v.map(|v| v.values.join(", "))
                    .unwrap_or_else(|| "不明".into())
            };
            let scope = match info.scope.as_str() {
                "catalog" => "カタログ値（接続先ごとに異なる場合があります）",
                "route" => "接続先の値",
                _ => "未対応の情報種別",
            };
            format!(
                "コンテキスト: {} / 入力上限: {} / 出力上限: {}\n入力: {} / 出力: {}\n{}\nコンテキストの取得元: {}",
                tokens(info.context_window),
                tokens(info.max_input_tokens),
                tokens(info.max_output_tokens),
                formats(info.input_modalities.as_option()),
                formats(info.output_modalities.as_option()),
                scope,
                info.field_sources
                    .get("context_window")
                    .unwrap_or(&info.source)
            )
        } else {
            "モデル情報は未取得、または提供元が公開していません".into()
        };
        state.set_model_info(text.into());
    }
}
