//! Application lifetime, navigation and event coordination, independent of pages.
use crate::{
    AppWindow, ChatEntry, ProviderField, SettingsDraft, Theme, UiState,
    application::demo::Mode,
    infrastructure::{avatar, tray},
    settings,
};
use ene_protocol::avatar::{AvatarEvent, MotionState};
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub struct Application {
    host: RefCell<Option<crate::infrastructure::host::Controller>>,
    monitor: RefCell<Option<crate::infrastructure::host::Controller>>,
    providers: RefCell<Vec<ene_api_types::provider::ProviderInfo>>,
    models: RefCell<Vec<ene_api_types::provider::Model>>,
    pub settings: RefCell<settings::State>,
    pub messages: Rc<VecModel<ChatEntry>>,
    path: PathBuf,
    mode: Cell<Mode>,
    snapshot: RefCell<Option<ene_api_types::host::Snapshot>>,
    host_drafts: RefCell<host::Drafts>,
    host_ui: RefCell<crate::host_model::HostUiModel>,
    availability: RefCell<Option<ene_api_types::host::ConnectionAvailability>>,
    availability_request: RefCell<host::AvailabilityRequest>,
    history: RefCell<Vec<ene_api_types::host::Message>>,
    conversation_drafts: RefCell<HashMap<String, String>>,
    avatar: Option<Rc<avatar::Controller>>,
    tray: Option<Rc<tray::Ui>>,
    registration: RefCell<Option<tray::Registration>>,
}
mod avatar_actions;
mod bindings;
mod conversation;
pub mod demo;
mod host;
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
            host: RefCell::new(None),
            monitor: RefCell::new(None),
            providers: RefCell::new(vec![]),
            models: RefCell::new(vec![]),
            settings: RefCell::new(settings::State::new(initial)),
            messages: Rc::new(VecModel::default()),
            path,
            mode: Cell::new(mode),
            snapshot: RefCell::new(None),
            host_drafts: RefCell::new(host::Drafts::default()),
            host_ui: RefCell::new(crate::host_model::HostUiModel::default()),
            availability: RefCell::new(None),
            availability_request: RefCell::new(host::AvailabilityRequest::Idle),
            history: RefCell::new(vec![]),
            conversation_drafts: RefCell::new(HashMap::new()),
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
                "このPCに会話を保存します。"
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
        if mode == Mode::Demo
            && let Some(tray) = &app.tray
        {
            tray.set_connection_status("デモ · 外部送信なし");
        }
        app.publish_settings(ui);
        crate::presentation::selectors::bind(ui);
        app.bind_actions(ui);
        if mode != Mode::Demo {
            app.start_monitor(ui);
        }
        app
    }
}
