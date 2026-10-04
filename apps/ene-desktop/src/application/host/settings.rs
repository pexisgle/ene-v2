use super::super::*;

impl Application {
    pub(in crate::application) fn reset_host_page(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
        if !state.get_host_connected() || state.get_host_busy() {
            return;
        }
        match state.get_category() {
            1 => state.set_companion_dirty(false),
            2 => {
                if let Some(host) = self.host.borrow().as_ref() {
                    host.discard_connection_draft(state.get_selected_companion_id().as_str());
                }
                state.set_connection_dirty(false);
                state.set_api_key("".into());
                state.set_credential_edit(0);
                state.set_provider_id("".into());
            }
            3 => state.set_permissions_dirty(false),
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
        state.set_host_edit_revision(state.get_host_edit_revision() + 1);
        state.set_host_feedback_error(false);
        state.set_host_feedback("元に戻しました。".into());
    }
    pub(in crate::application) fn host_request(
        &self,
        ui: &AppWindow,
        command: crate::infrastructure::host::Command,
    ) {
        let state = ui.global::<UiState>();
        let revoke = matches!(
            &command,
            crate::infrastructure::host::Command::RevokeCredential(_)
                | crate::infrastructure::host::Command::RevokePermissions(_)
        );
        if !state.get_host_connected() || (state.get_host_busy() && !revoke) {
            return;
        }
        if let Some(host) = self.host.borrow().as_ref() {
            let operation = match &command {
                crate::infrastructure::host::Command::Configure { .. } => 1,
                crate::infrastructure::host::Command::Companion { .. } => 2,
                crate::infrastructure::host::Command::Permission { .. }
                | crate::infrastructure::host::Command::RevokePermissions(_) => 3,
                crate::infrastructure::host::Command::History => 4,
                _ => 5,
            };
            state.set_host_save_category(match operation {
                1 => 2,
                2 => 1,
                3 => 3,
                _ => -1,
            });
            state.set_host_feedback_error(false);
            state.set_host_feedback("処理しています…".into());
            if let Some(id) = host.request_at(
                state.get_selected_companion_id().as_str(),
                command,
                state.get_host_edit_revision(),
            ) {
                state.set_host_request_id(id.into());
                state.set_host_operation(operation);
            } else {
                state.set_host_feedback_error(true);
                state.set_host_feedback(
                    "接続処理が停止しました。ホストに接続し直してください。".into(),
                );
            }
        }
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
                    .all(|f| !matches!(f.kind, ene_api_types::provider::SettingKind::Unknown(_))),
        );
        state.set_provider_state(p.state.clone().into());
        state.set_provider_error(p.error.clone().unwrap_or_default().into());
        state.set_requires_credentials(p.requires_credentials);
        state.set_environment_variable(p.environment_variable.clone().unwrap_or_default().into());
        state.set_model_list_supported(p.capabilities.model_list && p.state == "ready");
        state.set_destinations(p.destinations.join(" → ").into());
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
                            use ene_api_types::provider::SettingKind;
                            let value = p.options.get(&f.key);
                            ProviderField {
                                key: f.key.clone().into(),
                                label: f.label.clone().into(),
                                kind: match f.kind {
                                    SettingKind::Text => 0,
                                    SettingKind::Number => 1,
                                    SettingKind::Boolean => 2,
                                    SettingKind::Select => 3,
                                    SettingKind::Unknown(_) => 4,
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
            .and_then(|m| m.metadata.as_ref());
        let text = if let Some(info) = info {
            let tokens = |v: Option<u64>| {
                v.map(|v| format!("{v} tokens"))
                    .unwrap_or_else(|| "不明".into())
            };
            let formats = |v: &Option<Vec<String>>| {
                v.as_ref()
                    .map(|v| v.join(", "))
                    .unwrap_or_else(|| "不明".into())
            };
            let scope = match info.scope {
                ene_api_types::provider::MetadataScope::Catalog => {
                    "カタログ値（接続先ごとに異なる場合があります）"
                }
                ene_api_types::provider::MetadataScope::Route => "接続先の値",
                ene_api_types::provider::MetadataScope::Unknown(_) => "未対応の情報種別",
            };
            format!(
                "コンテキスト: {} / 入力上限: {} / 出力上限: {}\n入力: {} / 出力: {}\n{}\nコンテキストの取得元: {}",
                tokens(info.context_window),
                tokens(info.max_input_tokens),
                tokens(info.max_output_tokens),
                formats(&info.input_modalities),
                formats(&info.output_modalities),
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
