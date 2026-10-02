//! Application lifetime, navigation and event coordination, independent of pages.
use crate::{
    AppWindow, ChatEntry, ProviderField, SettingsDraft, Theme, UiState, avatar, chat::Mode,
    settings, tray,
};
use ene_protocol::avatar::{AvatarEvent, MotionState};
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub struct Application {
    host: RefCell<Option<crate::host::Controller>>,
    providers: RefCell<Vec<ene_api_types::provider::ProviderInfo>>,
    models: RefCell<Vec<ene_api_types::provider::Model>>,
    pub settings: RefCell<settings::State>,
    pub messages: Rc<VecModel<ChatEntry>>,
    path: PathBuf,
    mode: Mode,
    avatar: Option<Rc<avatar::Controller>>,
    tray: Option<Rc<tray::Ui>>,
    registration: RefCell<Option<tray::Registration>>,
}
impl Application {
    pub fn bind(
        ui: &AppWindow,
        mode: Mode,
        path: PathBuf,
        initial: settings::ClientSettings,
        avatar: Option<Rc<avatar::Controller>>,
        tray: Option<Rc<tray::Ui>>,
    ) -> Rc<Self> {
        let app = Rc::new(Self {
            host: RefCell::new(None),
            providers: RefCell::new(vec![]),
            models: RefCell::new(vec![]),
            settings: RefCell::new(settings::State::new(initial)),
            messages: Rc::new(VecModel::default()),
            path,
            mode,
            avatar,
            tray,
            registration: RefCell::new(None),
        });
        ui.global::<UiState>().set_demo_mode(mode == Mode::Demo);
        ui.global::<UiState>()
            .set_messages(app.messages.clone().into());
        ui.global::<UiState>().set_status(
            if mode == Mode::Demo {
                "返答は仮応答です。"
            } else {
                "このPCのホストを選んで接続してください。"
            }
            .into(),
        );
        app.publish_settings(ui);
        app.bind_actions(ui);
        app
    }
    fn publish_settings(&self, ui: &AppWindow) {
        let state = self.settings.borrow();
        let view = ui.global::<UiState>();
        view.set_font_family(state.saved.font_family.clone().into());
        view.set_text_size(state.saved.text_size);
        view.set_avatar_visible(state.saved.avatar.visible);
        view.set_settings_draft(project(&state.draft));
        view.set_settings_dirty(state.dirty());
        ui.global::<Theme>().invoke_apply(state.saved.theme_index);
        if let Some(tray) = &self.tray {
            tray.set_avatar_visible(state.saved.avatar.visible);
        }
    }
    fn bind_actions(self: &Rc<Self>, ui: &AppWindow) {
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
        let owner = Rc::downgrade(self);
        let weak = ui.as_weak();
        ui.global::<UiState>().on_draft_edited(move |_| {
            if let Some(ui) = weak.upgrade()
                && let Some(app) = owner.upgrade()
            {
                app.refresh_send(&ui);
            }
        });
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.on_host_events_available(move || {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                app.drain_host(&ui);
            }
        });
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_start_host(move || {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                app.start_host(&ui);
            }
        });
        macro_rules! host_action {
            ($on:ident,$command:expr) => {{
                let owner = Rc::downgrade(self);
                let view = ui.as_weak();
                ui.global::<UiState>().$on(move || {
                    if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                        app.host_request(&ui, ($command)(&ui));
                    }
                });
            }};
        }
        let owner = Rc::downgrade(self);
        let view = ui.as_weak();
        ui.global::<UiState>().on_choose_provider(move |index| {
            if let (Some(app), Some(ui)) = (owner.upgrade(), view.upgrade()) {
                app.project_provider(&ui, index, true);
                ui.global::<UiState>()
                    .set_connection_matches_provider(false);
                let state = ui.global::<UiState>();
                state.set_selected_connection_id("".into());
                state.set_credential_state(0);
                state.set_allow_conversation(false);
                state.set_allow_check(false);
                state.set_host_dirty(true);
            }
        });
        host_action!(on_configure_host, |ui: &AppWindow| {
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
            crate::host::Command::Configure {
                provider: state.get_provider_id().into(),
                endpoint: state.get_provider_endpoint().into(),
                model: state.get_model_id().into(),
                options: serde_json::Value::Object(options),
            }
        });
        host_action!(on_reload_providers, |_| {
            crate::host::Command::ReloadProviders
        });
        host_action!(on_toggle_provider, |ui: &AppWindow| {
            let s = ui.global::<UiState>();
            crate::host::Command::EnableProvider {
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
        host_action!(on_fetch_models, |ui: &AppWindow| {
            crate::host::Command::Models(ui.global::<UiState>().get_selected_connection_id().into())
        });
        host_action!(on_register_credential, |ui: &AppWindow| {
            let state = ui.global::<UiState>();
            let secret = state.get_api_key().to_string();
            state.set_api_key("".into());
            crate::host::Command::Credential(state.get_selected_connection_id().into(), secret)
        });
        host_action!(on_environment_credential, |ui: &AppWindow| {
            crate::host::Command::EnvironmentCredential(
                ui.global::<UiState>().get_selected_connection_id().into(),
            )
        });
        host_action!(on_revoke_credential, |ui: &AppWindow| {
            crate::host::Command::RevokeCredential(
                ui.global::<UiState>().get_selected_connection_id().into(),
            )
        });
        host_action!(on_save_permissions, |ui: &AppWindow| {
            let state = ui.global::<UiState>();
            crate::host::Command::Permission {
                connection: state.get_selected_connection_id().into(),
                conversation: state.get_allow_conversation(),
                check: state.get_allow_check(),
            }
        });
        host_action!(on_check_connection, |ui: &AppWindow| {
            crate::host::Command::Check(ui.global::<UiState>().get_selected_connection_id().into())
        });
        host_action!(on_save_companion, |ui: &AppWindow| {
            let state = ui.global::<UiState>();
            crate::host::Command::Companion {
                name: state.get_companion_name().into(),
                mind: state.get_companion_mind().into(),
            }
        });
        host_action!(on_load_history, |_| crate::host::Command::History);
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
                ui.global::<UiState>().set_tray_status(
                    "トレイを利用できないため画面を表示したままにします。ホスト選択画面から終了できます。"
                        .into(),
                );
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
            let view = ui.as_weak();
            tray.on_quit(move || {
                if let Some(ui) = view.upgrade() {
                    show_host_selection(&ui);
                }
            });
        }
    }
    pub fn edit_settings(&self, ui: &AppWindow) {
        let draft = read_draft(ui.global::<UiState>().get_settings_draft());
        let mut state = self.settings.borrow_mut();
        state.edit(draft);
        ui.global::<UiState>().set_settings_dirty(state.dirty());
        ui.global::<UiState>().set_save_status("".into());
    }
    pub fn save_settings(&self, ui: &AppWindow) {
        self.edit_settings(ui);
        let result = self.settings.borrow_mut().save_draft(&self.path);
        match result {
            Ok(()) => {
                self.publish_settings(ui);
                if let Some(avatar) = &self.avatar {
                    avatar.configure(self.settings.borrow().saved.avatar.clone());
                }
                let view = ui.global::<UiState>();
                view.set_apply_error("".into());
                view.set_error_field("".into());
                view.set_save_status("保存しました。".into());
            }
            Err(error) => {
                let view = ui.global::<UiState>();
                view.set_apply_error(error.message.into());
                view.set_error_field(error.field.into());
                view.set_save_status("".into());
                view.set_category(0);
                ui.invoke_focus_error();
            }
        }
    }
    pub fn reset_settings(&self, ui: &AppWindow) {
        self.settings.borrow_mut().reset();
        self.publish_settings(ui);
        let view = ui.global::<UiState>();
        view.set_apply_error("".into());
        view.set_error_field("".into());
        view.set_save_status("変更を戻しました。".into());
    }
    fn start_demo(&self, ui: &AppWindow) {
        if self.mode != Mode::Demo || ui.global::<UiState>().get_quit_confirmation() {
            return;
        }
        ui.global::<UiState>().set_session_active(true);
        navigate(ui, 0);
    }
    fn submit(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        if view.get_page() != 0
            || view.get_host_selection_visible()
            || !view.get_session_active()
            || view.get_quit_confirmation()
        {
            return;
        }
        let draft = view.get_draft();
        if self.mode != Mode::Demo {
            self.refresh_send(ui);
            if !view.get_can_send() {
                return;
            }
            if let Some(host) = self.host.borrow().as_ref() {
                view.set_send_pending(true);
                host.request(crate::host::Command::Send(draft.into()));
                self.refresh_send(ui);
            }
            return;
        }
        let Some(response) = self.mode.respond(&draft) else {
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
    pub fn request_quit(&self, ui: &AppWindow) {
        // Capture any edit even if the event-loop's edit notification is pending.
        self.edit_settings(ui);
        if self.settings.borrow().dirty() {
            ui.global::<UiState>().set_quit_confirmation(true);
            show(ui);
            ui.invoke_focus_quit();
        } else {
            quit();
        }
    }
    fn persist_live(&self, ui: &AppWindow, updated: settings::ClientSettings) -> bool {
        if let Err(error) = updated.save(&self.path) {
            ui.global::<UiState>().set_apply_error(error.into());
            ui.global::<UiState>().set_error_field("".into());
            return false;
        }
        self.settings.borrow_mut().accept_live(updated);
        self.publish_settings(ui);
        if let Some(avatar) = &self.avatar {
            avatar.configure(self.settings.borrow().saved.avatar.clone());
        }
        true
    }
    pub fn toggle_avatar(&self, ui: &AppWindow) {
        let mut updated = self.settings.borrow().saved.clone();
        updated.avatar.visible = !updated.avatar.visible;
        self.persist_live(ui, updated);
    }
    pub fn recenter_avatar(&self, ui: &AppWindow) {
        let mut updated = self.settings.borrow().saved.clone();
        updated.avatar.visible = true;
        if self.persist_live(ui, updated)
            && let Some(avatar) = &self.avatar
        {
            avatar.recenter();
        }
    }
    fn restart_avatar(&self, ui: &AppWindow) {
        if ui.global::<UiState>().get_avatar_failed()
            && let Some(avatar) = &self.avatar
        {
            ui.global::<UiState>().set_avatar_ready(false);
            ui.global::<UiState>()
                .set_avatar_status("描画を再起動しています…".into());
            avatar.restart(self.settings.borrow().saved.avatar.clone());
        }
    }
    pub fn selection_result(
        &self,
        ui: &AppWindow,
        generation: u64,
        motion: bool,
        result: Result<Option<String>, String>,
    ) {
        ui.global::<UiState>().set_picker_active(false);
        if generation != self.settings.borrow().generation {
            return;
        }
        match result {
            Ok(Some(path)) => {
                self.settings
                    .borrow_mut()
                    .selection(generation, motion, path);
                self.publish_settings(ui);
            }
            Ok(None) => {}
            Err(error) => {
                ui.global::<UiState>().set_apply_error(error.into());
                ui.global::<UiState>().set_error_field("".into());
            }
        }
    }
    fn tray_ready(&self) -> bool {
        self.registration
            .borrow()
            .as_ref()
            .is_some_and(|r| r.usable_at(Instant::now()))
    }
    pub fn start_services(
        self: &Rc<Self>,
        ui: &AppWindow,
        receiver: Receiver<tray::Registration>,
    ) -> [slint::Timer; 2] {
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
                            "トレイを利用できません。画面を開いたまま使用できます。"
                        }
                        .into(),
                    );
                    if !ready && !ui.window().is_visible() {
                        show(&ui);
                    }
                }
            },
        );
        let fallback = slint::Timer::default();
        let view = ui.as_weak();
        fallback.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(ui) = view.upgrade() {
                    ui.invoke_avatar_events_available();
                    ui.invoke_host_events_available();
                }
            },
        );
        if let Some(avatar) = &self.avatar {
            avatar.configure(self.settings.borrow().saved.avatar.clone());
        }
        ui.global::<UiState>()
            .set_avatar_status("キャラクターを読み込んでいます…".into());
        [monitor, fallback]
    }
    fn drain_avatar(&self, ui: &AppWindow) {
        let Some(avatar) = &self.avatar else {
            return;
        };
        for event in avatar.events.try_iter() {
            self.handle_avatar_event(ui, event);
        }
    }
    pub fn handle_avatar_event(&self, ui: &AppWindow, event: AvatarEvent) {
        let view = ui.global::<UiState>();
        match event {
            AvatarEvent::Ready { .. } => {
                view.set_avatar_ready(true);
                view.set_avatar_failed(false);
                view.set_avatar_status("キャラクターを表示しています。".into());
            }
            AvatarEvent::Error { message } => {
                view.set_avatar_ready(false);
                view.set_avatar_failed(true);
                view.set_avatar_status(message.into());
            }
            AvatarEvent::MotionError { message } => {
                view.set_motion_failed(true);
                view.set_motion_status(message.into());
            }
            AvatarEvent::MotionState { state } => {
                let text = match state {
                    MotionState::None if self.settings.borrow().saved.avatar.motion.is_some() => {
                        None
                    }
                    MotionState::None => Some("静止姿勢です。"),
                    MotionState::Loading => Some("モーションを読み込んでいます…"),
                    MotionState::Playing => Some("モーションを再生しています。"),
                    MotionState::Paused => Some("モーションは一時停止中です。"),
                    MotionState::Finished => Some("モーションの再生が完了しました。"),
                };
                if let Some(text) = text {
                    view.set_motion_failed(false);
                    view.set_motion_status(text.into());
                }
            }
            AvatarEvent::ShowChat => navigate(ui, 0),
            AvatarEvent::PositionChanged {
                right,
                bottom,
                output,
            } => {
                let mut updated = self.settings.borrow().saved.clone();
                updated.avatar.right = right;
                updated.avatar.bottom = bottom;
                updated.avatar.output = output;
                // The renderer already moved; acknowledge without reconfiguring it.
                match updated.save(&self.path) {
                    Ok(()) => {
                        self.settings.borrow_mut().accept_live(updated);
                        self.publish_settings(ui);
                    }
                    Err(error) => {
                        view.set_avatar_status(error.into());
                        if let Some(avatar) = &self.avatar {
                            avatar.configure(self.settings.borrow().saved.avatar.clone());
                        }
                    }
                }
            }
            AvatarEvent::VisibilityChanged { visible } => {
                if !view.get_avatar_failed() {
                    view.set_avatar_status(
                        if visible {
                            "キャラクターを表示しています。"
                        } else {
                            "キャラクターは非表示です。"
                        }
                        .into(),
                    );
                }
                if let Some(tray) = &self.tray {
                    tray.set_avatar_visible(visible);
                }
            }
            AvatarEvent::Stopped => view.set_avatar_ready(false),
        }
        if let Some(tray) = &self.tray {
            tray.set_avatar_failed(view.get_avatar_failed());
        }
    }
}
fn project(d: &settings::Draft) -> SettingsDraft {
    SettingsDraft {
        font_family: d.font_family.clone().into(),
        text_size: d.text_size,
        theme_index: d.theme_index,
        model: d.model.clone().into(),
        motion: d.motion.clone().into(),
        motion_playing: d.motion_playing,
        motion_loop: d.motion_loop,
        fps_index: d.fps_index,
        size_text: d.size_text.clone().into(),
    }
}
fn read_draft(d: SettingsDraft) -> settings::Draft {
    settings::Draft {
        font_family: d.font_family.to_string(),
        text_size: d.text_size,
        theme_index: d.theme_index,
        model: d.model.to_string(),
        motion: d.motion.to_string(),
        motion_playing: d.motion_playing,
        motion_loop: d.motion_loop,
        fps_index: d.fps_index,
        size_text: d.size_text.to_string(),
    }
}
fn show(ui: &AppWindow) {
    if let Err(error) = ui.show() {
        eprintln!("画面を表示できません: {error}");
    }
    ui.window().set_minimized(false);
    ui.window().request_redraw();
}
fn focus_page(ui: &AppWindow) {
    ui.invoke_focus_entry();
}
pub fn show_host_selection(ui: &AppWindow) {
    if ui.global::<UiState>().get_quit_confirmation() {
        show(ui);
        return;
    }
    ui.global::<UiState>().set_host_selection_visible(true);
    show(ui);
    focus_page(ui);
}
pub fn navigate(ui: &AppWindow, page: i32) {
    if ui.global::<UiState>().get_quit_confirmation() {
        show(ui);
        return;
    }
    let page = page.clamp(0, 1);
    if page == 0 && !ui.global::<UiState>().get_session_active() {
        show_host_selection(ui);
        return;
    }
    ui.global::<UiState>().set_host_selection_visible(false);
    ui.global::<UiState>().set_page(page.clamp(0, 1));
    show(ui);
    focus_page(ui);
}
fn quit() {
    if let Err(error) = slint::quit_event_loop() {
        eprintln!("クライアントを終了できません: {error}");
    }
}

impl Application {
    fn start_host(self: &Rc<Self>, ui: &AppWindow) {
        if self.mode == Mode::Demo {
            return;
        }
        if self.host.borrow().is_none() {
            let view = ui.as_weak();
            *self.host.borrow_mut() = Some(crate::host::Controller::new(move || {
                let _ = view.upgrade_in_event_loop(|ui| ui.invoke_host_events_available());
            }));
        }
        ui.global::<UiState>().set_session_active(true);
        navigate(ui, 0);
    }
    fn host_request(&self, ui: &AppWindow, command: crate::host::Command) {
        if let Some(host) = self.host.borrow().as_ref() {
            ui.global::<UiState>()
                .set_host_feedback("処理しています…".into());
            host.request(command);
        }
    }
    fn refresh_send(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        let ready = if self.mode == Mode::Demo {
            true
        } else {
            view.get_host_connected() && view.get_chat_ready() && !view.get_send_pending()
        };
        view.set_can_send(ready && !view.get_draft().trim().is_empty());
    }
    fn project_provider(&self, ui: &AppWindow, index: i32, reset: bool) {
        let providers = self.providers.borrow();
        let Some(p) = providers.get(index.max(0) as usize) else {
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
        state.set_provider_ready(p.state == "ready");
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
    }
    fn project_model_info(&self, ui: &AppWindow) {
        let state = ui.global::<UiState>();
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
    fn drain_host(&self, ui: &AppWindow) {
        use crate::host::Event;
        let state = ui.global::<UiState>();
        let host = self.host.borrow();
        let Some(host) = host.as_ref() else {
            return;
        };
        for event in host.events.try_iter() {
            match event {
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
                    self.project_provider(ui, index, !state.get_host_dirty() && current.is_empty());
                }
                Event::Models(id, list) => {
                    if id != state.get_selected_connection_id().as_str()
                        || !state.get_connection_matches_provider()
                    {
                        continue;
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
                    let Some(c) = snapshot.companions.first() else {
                        continue;
                    };
                    let ready = c.availability["chat"]["ready"] == true;
                    if state.get_host_connected() && state.get_chat_ready() != ready {
                        state.set_status(if ready {"対話できます。"}else{"対話の準備が必要です。設定で接続・資格情報・送信許可を確認してください。"}.into());
                    }
                    state.set_chat_ready(ready);
                    state.set_has_older(
                        snapshot
                            .message_cursors
                            .get(&c.id)
                            .is_some_and(|c| c.is_some()),
                    );
                    state.set_connection_configured(!snapshot.settings.connections.is_empty());
                    let assignment = c.model_assignments.companion.as_ref();
                    let selected = snapshot
                        .settings
                        .connections
                        .iter()
                        .find(|p| p.adapter == state.get_provider_id().as_str())
                        .or_else(|| {
                            assignment.and_then(|a| {
                                snapshot
                                    .settings
                                    .connections
                                    .iter()
                                    .find(|p| p.id == a.connection_id)
                            })
                        })
                        .or_else(|| snapshot.settings.connections.first());
                    state.set_selected_connection_id(
                        selected.map(|p| p.id.clone()).unwrap_or_default().into(),
                    );
                    state.set_connection_matches_provider(selected.is_some_and(|p| {
                        p.adapter == state.get_provider_id().as_str()
                            && p.endpoint == state.get_provider_endpoint().as_str()
                    }));
                    state.set_credential_state(
                        match selected.map(|p| p.credential_state.as_str()) {
                            Some("stored") => 1,
                            Some("session_only") => 2,
                            _ => 0,
                        },
                    );
                    if !state.get_host_dirty()
                        && let Some(p) = selected
                    {
                        state.set_provider_id(p.adapter.clone().into());
                        if let Some(index) = self
                            .providers
                            .borrow()
                            .iter()
                            .position(|d| d.id == p.adapter)
                        {
                            self.project_provider(ui, index as i32, false);
                        }
                        state.set_provider_endpoint(p.endpoint.clone().into());
                        state.set_model_id(
                            assignment
                                .filter(|a| a.connection_id == p.id)
                                .map(|a| a.model_id.as_str())
                                .unwrap_or_default()
                                .into(),
                        );
                        let fields = state.get_provider_fields();
                        for (row, mut f) in fields.iter().enumerate() {
                            if let Some(v) = p.options.get(f.key.as_str()) {
                                f.value = v
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
                                    .into();
                                f.checked = v.as_bool().unwrap_or(false);
                                fields.set_row_data(row, f);
                            }
                        }
                    }
                    if !state.get_host_dirty() {
                        state.set_companion_name(c.name.clone().into());
                        state.set_companion_mind(c.mind.clone().into());
                        state.set_allow_conversation(snapshot.grants.iter().any(|g| {
                            g.status == "active"
                                && g.purpose == "conversation"
                                && selected.is_some_and(|p| p.id == g.connection_id)
                        }));
                        state.set_allow_check(snapshot.grants.iter().any(|g| {
                            g.status == "active"
                                && g.purpose == "connection_check"
                                && selected.is_some_and(|p| p.id == g.connection_id)
                        }));
                    }
                    if !state.get_host_dirty() {
                        state.set_connection_matches_provider(selected.is_some_and(|p| {
                            p.adapter == state.get_provider_id().as_str()
                                && p.endpoint == state.get_provider_endpoint().as_str()
                        }));
                    }
                    self.project_model_info(ui);
                    let entries = messages
                        .into_iter()
                        .map(|m| {
                            use ene_api_types::host::{MessageStatus, Role};
                            let author = if m.role == Role::User {
                                "あなた".to_owned()
                            } else {
                                c.name.clone()
                            };
                            let message_state = match m.status {
                                MessageStatus::Queued | MessageStatus::Streaming => 1,
                                MessageStatus::Failed => 2,
                                MessageStatus::Cancelled => 3,
                                _ => 0,
                            };
                            let mut body = m.text;
                            if let Some(failure) = m.failure {
                                body.push_str(&format!("\n{}", failure.message));
                            }
                            ChatEntry {
                                author: author.into(),
                                body: body.into(),
                                provisional: false,
                                state: message_state,
                                outgoing: m.role == Role::User,
                            }
                        })
                        .collect::<Vec<_>>();
                    self.messages.set_vec(entries);
                }
                Event::Status(text, connected) => {
                    state.set_host_connected(connected);
                    state.set_status(if connected && !state.get_chat_ready() {
                        "接続しました。設定で接続・資格情報・送信許可を確認してください。".into()
                    } else {
                        text.into()
                    });
                }
                Event::Accepted(text) => {
                    state.set_conversation_error("".into());
                    state.set_send_pending(false);
                    if state.get_draft() == text {
                        state.set_draft("".into());
                    }
                    ui.invoke_focus_input();
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
                    state.set_host_feedback_error(true);
                    state.set_host_feedback(text.into());
                }
                Event::Done(text, committed) => {
                    state.set_host_feedback_error(false);
                    if committed {
                        state.set_host_dirty(false);
                    }
                    state.set_host_feedback(text.into());
                }
                Event::Older(page) => state.set_has_older(page.next_cursor.is_some()),
            }
        }
        self.refresh_send(ui);
    }
}
