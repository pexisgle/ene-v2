//! Application lifetime, navigation and event coordination, independent of pages.
use crate::{
    AppWindow, ChatEntry, SettingsDraft, Theme, UiState, avatar, chat::Mode, settings, tray,
};
use ene_shared::avatar::{AvatarEvent, MotionState};
use slint::{ComponentHandle, VecModel};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub struct Application {
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
                "ホストへの画面接続は準備中です。"
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
        let mode = self.mode;
        let weak = ui.as_weak();
        ui.global::<UiState>().on_draft_edited(move |text| {
            if let Some(ui) = weak.upgrade() {
                ui.global::<UiState>().set_can_send(mode.accepts(&text));
            }
        });
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
