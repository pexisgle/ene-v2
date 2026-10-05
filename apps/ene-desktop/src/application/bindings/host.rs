use super::super::*;

impl Application {
    pub(super) fn bind_host_settings(self: &Rc<Self>, ui: &AppWindow) {
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_reset_host_drafts(move || {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                let state = ui.global::<UiState>();
                if !state.get_host_connected() || state.get_host_busy() {
                    return;
                }
                if let Some(host) = app.host.borrow().as_ref() {
                    host.discard_connection_draft(state.get_selected_companion_id().as_str());
                }
                state.set_companion_dirty(false);
                state.set_connection_dirty(false);
                state.set_permissions_dirty(false);
                state.set_api_key("".into());
                state.set_credential_edit(0);
                state.set_provider_id("".into());
                let snapshot = app.snapshot.borrow().clone();
                let messages = app.history.borrow().clone();
                if let Some(snapshot) = snapshot {
                    app.handle_host_event(
                        &ui,
                        crate::infrastructure::host::Event::View(Box::new(snapshot), messages),
                    );
                }
                state.set_host_edit_revision(state.get_host_edit_revision() + 1);
                state.set_host_feedback_error(false);
                state.set_host_feedback("未保存のホスト設定を元に戻しました。".into());
            }
        });
        macro_rules! host_action {
            ($on:ident,$command:expr) => {{
                let owner = Rc::downgrade(self);
                let view = ui.as_weak();
                ui.global::<UiState>().$on(move || {
                    if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                        if ui.global::<UiState>().get_host_connected()
                            && ui.global::<UiState>().get_companion_index() >= 0
                        {
                            app.host_request(&ui, ($command)(&app, &ui));
                        }
                    }
                });
            }};
        }
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_choose_provider(move |index| {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                if ui.global::<UiState>().get_host_busy()
                    || ui.global::<UiState>().get_permissions_dirty()
                {
                    return;
                }
                app.project_provider(&ui, index, true);
                ui.global::<UiState>()
                    .set_connection_matches_provider(false);
                let state = ui.global::<UiState>();
                if let Some(host) = app.host.borrow().as_ref() {
                    host.discard_connection_draft(state.get_selected_companion_id().as_str());
                }
                state.set_selected_connection_id("".into());
                state.set_credential_state(0);
                state.set_saved_model(false);
                state.set_allow_conversation(false);
                state.set_allow_check(false);
                state.set_connection_dirty(true);
                state.set_host_edit_revision(state.get_host_edit_revision() + 1);
                state.set_api_key("".into());
                state.set_credential_edit(0);
            }
        });
        host_action!(on_configure_host, |app: &Application, ui: &AppWindow| {
            let state = ui.global::<UiState>();
            let mut options = serde_json::Map::new();
            for f in state.get_provider_fields().iter() {
                let v = match f.kind {
                    1 => f
                        .value
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                    2 => serde_json::json!(f.checked),
                    _ => serde_json::json!(f.value.as_str()),
                };
                options.insert(f.key.to_string(), v);
            }
            // Provider-owned opaque defaults (e.g. builtin routing constraints) are retained.
            crate::infrastructure::host::Command::Configure {
                settings_revision: app.host_drafts.borrow().settings_revision,
                companion_revision: app.host_drafts.borrow().assignment_revision,
                creation_id: None,
                connection_id: (!state.get_selected_connection_id().is_empty())
                    .then(|| state.get_selected_connection_id().to_string()),
                provider: state.get_provider_id().into(),
                endpoint: state.get_provider_endpoint().into(),
                model: state.get_model_id().into(),
                options: serde_json::Value::Object(options),
                update_connection: state.get_connection_dirty()
                    || !state.get_connection_matches_provider(),
                credential: if !state.get_requires_credentials() {
                    None
                } else {
                    match state.get_credential_edit() {
                        1 => Some(crate::infrastructure::host::CredentialEdit::Environment),
                        2 => Some(crate::infrastructure::host::CredentialEdit::Revoke),
                        _ if !state.get_api_key().is_empty() => {
                            Some(crate::infrastructure::host::CredentialEdit::Secret(
                                state.get_api_key().into(),
                            ))
                        }
                        _ => None,
                    }
                },
            }
        });
        host_action!(on_reload_providers, |_: &Application, _: &AppWindow| {
            crate::infrastructure::host::Command::ReloadProviders
        });
        host_action!(on_toggle_provider, |_app: &Application, ui: &AppWindow| {
            let s = ui.global::<UiState>();
            crate::infrastructure::host::Command::EnableProvider {
                provider: s.get_provider_id().into(),
                enabled: !s.get_provider_ready(),
            }
        });
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_model_changed(move || {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                app.project_model_info(&ui);
            }
        });
        host_action!(on_fetch_models, |_app: &Application, ui: &AppWindow| {
            crate::infrastructure::host::Command::Models(
                ui.global::<UiState>().get_selected_connection_id().into(),
            )
        });
        host_action!(
            on_register_credential,
            |_app: &Application, ui: &AppWindow| {
                let state = ui.global::<UiState>();
                let secret = state.get_api_key().to_string();
                state.set_api_key("".into());
                crate::infrastructure::host::Command::Credential(
                    state.get_selected_connection_id().into(),
                    secret,
                )
            }
        );
        host_action!(
            on_environment_credential,
            |_app: &Application, ui: &AppWindow| {
                crate::infrastructure::host::Command::EnvironmentCredential(
                    ui.global::<UiState>().get_selected_connection_id().into(),
                )
            }
        );
        host_action!(
            on_revoke_credential,
            |_app: &Application, ui: &AppWindow| {
                crate::infrastructure::host::Command::RevokeCredential(
                    ui.global::<UiState>().get_selected_connection_id().into(),
                )
            }
        );
        host_action!(
            on_revoke_permissions,
            |_app: &Application, ui: &AppWindow| {
                crate::infrastructure::host::Command::RevokePermissions(
                    ui.global::<UiState>().get_selected_connection_id().into(),
                )
            }
        );
        host_action!(on_save_permissions, |app: &Application, ui: &AppWindow| {
            let state = ui.global::<UiState>();
            crate::infrastructure::host::Command::Permission {
                consent: app
                    .host_drafts
                    .borrow()
                    .permission
                    .as_ref()
                    .map(|p| p.consent.clone())
                    .unwrap_or(ene_api_types::GrantConsent {
                        settings_revision: 0,
                        provider_revision: String::new(),
                    }),
                destinations: app
                    .host_drafts
                    .borrow()
                    .permission
                    .as_ref()
                    .map(|p| p.destinations.clone())
                    .unwrap_or_default(),
                connection: state.get_selected_connection_id().into(),
                conversation: state.get_allow_conversation(),
                check: state.get_allow_check(),
            }
        });
        host_action!(on_check_connection, |_app: &Application, ui: &AppWindow| {
            crate::infrastructure::host::Command::Check(
                ui.global::<UiState>().get_selected_connection_id().into(),
            )
        });
        host_action!(on_save_companion, |app: &Application, ui: &AppWindow| {
            let state = ui.global::<UiState>();
            crate::infrastructure::host::Command::Companion {
                revision: app.host_drafts.borrow().companion_revision,
                name: state.get_companion_name().into(),
                mind: state.get_companion_mind().into(),
            }
        });
        host_action!(on_load_history, |_: &Application, _: &AppWindow| {
            crate::infrastructure::host::Command::History
        });
    }
}
