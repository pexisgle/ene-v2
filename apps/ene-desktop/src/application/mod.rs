//! Application lifetime, navigation and event coordination, independent of pages.
use crate::{
    AppWindow, ChatEntry, SettingsDraft, Theme, UiState,
    application::demo::Mode,
    infrastructure::{avatar, tray},
    settings,
};
use ene_protocol::avatar::{AvatarEvent, MotionState};
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
mod avatar_actions;
mod bindings;
mod conversation;
pub mod demo;
mod lifecycle;
mod navigation;
mod settings_actions;

use lifecycle::quit;
use navigation::{focus_page, show};
pub use navigation::{navigate, show_host_selection};

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
        ui.global::<UiState>()
            .set_app_version(env!("CARGO_PKG_VERSION").into());
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
        let saved_font = app.settings.borrow().saved.font_family.clone();
        // Use the same font collection as Slint; the API is pinned with Slint 1.18.1.
        let mut fonts = slint::fontique_011::shared_collection();
        let mut families: Vec<String> = fonts.family_names().map(str::to_owned).collect();
        if !families.contains(&saved_font) {
            ui.global::<UiState>()
                .set_missing_font(saved_font.clone().into());
            families.push(saved_font);
        }
        families.sort_unstable_by_key(|name| name.to_lowercase());
        families.dedup();
        ui.global::<UiState>().set_font_families(
            Rc::new(VecModel::from(
                families
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
        );
        crate::presentation::selectors::bind(ui);
        app.publish_settings(ui);
        app.bind_actions(ui);
        app
    }
}
