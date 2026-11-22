//! Shared UI hub and window modules for the desktop companion.
//!
//! `Hub` is the single UI-thread entity holding the display copy of host state,
//! device status and async RPC helpers. Both the frameless transparent floating
//! `CompanionPod` and the standalone `SettingsStudio` window observe `Hub`.

mod companions;
mod connections;
mod device;
mod hub_actions;
mod hub_companions;
mod hub_connections;
mod hub_presets;
pub mod icons;
mod mcp_add;
pub mod pod;
pub mod studio;
pub mod theme;
mod voice;
mod widgets;
mod window_manager;

pub use icons::EmbeddedAssets;
pub use pod::{
    CompanionPod, Desktop, bind, focus_chat, initial_pod_bounds, initial_pod_bounds_for_mode,
};
pub use studio::SettingsStudio;
pub use window_manager::open_pod_window;

pub(crate) use ene_client_core::domain::{CompanionProfilePatch, NewCompanionSpec};
pub(crate) use ene_client_core::presets::SearchPresetSpec;
pub(crate) use ene_client_core::{
    GOOGLE_AGENT_PLATFORM, ModelRoleSlot, default_embedding_dimensions, search_adapter_requires_key,
};

use crate::{
    rpc::RpcDispatcher,
    session::{Connection, Options},
    state::Model,
    tray::{TrayAction, TrayHandle},
    update::{Command, Update},
};
use gpui::{AppContext as _, Context, Pixels, Size, Task, WeakEntity, WindowHandle, px, size};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug)]
pub struct HistoryReset {
    pub requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub failure: Option<String>,
}

impl gpui::Global for HistoryReset {}

/// Display mode of the frameless floating companion pod.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PodMode {
    #[default]
    MiniVoice,
    ExpandedChat,
}

impl PodMode {
    pub fn size(self) -> Size<Pixels> {
        match self {
            Self::MiniVoice => size(px(444.), px(218.)),
            Self::ExpandedChat => size(px(444.), px(540.)),
        }
    }

    pub fn min_size(self) -> Size<Pixels> {
        match self {
            Self::MiniVoice => size(px(360.), px(188.)),
            Self::ExpandedChat => size(px(380.), px(380.)),
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::MiniVoice => Self::ExpandedChat,
            Self::ExpandedChat => Self::MiniVoice,
        }
    }
}

/// The chapters inside `SettingsStudio`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    #[default]
    Companions,
    Connections,
    Voice,
    Device,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::Companions,
        SettingsSection::Connections,
        SettingsSection::Voice,
        SettingsSection::Device,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Companions => "コンパニオン",
            Self::Connections => "接続",
            Self::Voice => "音声テスト",
            Self::Device => "この端末",
        }
    }

    pub fn icon(self) -> icons::Icon {
        match self {
            Self::Companions => icons::Icon::Sparkles,
            Self::Connections => icons::Icon::Plug,
            Self::Voice => icons::Icon::Mic,
            Self::Device => icons::Icon::Sliders,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Companions => "studio-companions",
            Self::Connections => "studio-connections",
            Self::Voice => "studio-voice",
            Self::Device => "studio-device",
        }
    }
}

/// Dependencies and channel endpoints used to construct [`Hub`].
#[derive(Debug)]
pub struct HubInit {
    pub options: Options,
    pub connection: Connection,
    pub commands: UnboundedSender<Command>,
    pub updates_tx: UnboundedSender<Update>,
    pub updates: UnboundedReceiver<Update>,
    pub tray: Option<TrayHandle>,
    pub tray_rx: Option<UnboundedReceiver<TrayAction>>,
    pub runtime: tokio::runtime::Handle,
    pub discovery: String,
}

/// Single shared state hub observed by both `CompanionPod` and `SettingsStudio`.
pub struct Hub {
    pub model: Model,
    pub options: Options,
    pub(super) rpc: RpcDispatcher,
    pub(super) commands: UnboundedSender<Command>,
    pub pod_mode: PodMode,
    pub settings_section: SettingsSection,
    pub settings_window: Option<WindowHandle<SettingsStudio>>,
    pub pod_window: Option<WindowHandle<CompanionPod>>,
    pub(super) pod_view: Option<WeakEntity<CompanionPod>>,
    pub(super) pod_visible: bool,
    pub pod_scroll_ping: u64,
    pub cleared_secret_for: Option<(String, u64)>,
    pub created_ping: u64,
    pub(super) setup_pending: bool,
    pub(super) setup_error: Option<&'static str>,
    pub setup_error_detail: Option<String>,
    pub pending_setup_connection_id: Option<String>,
    pub pending_setup_models: Vec<String>,
    pub checking_connections: HashSet<String>,
    pub(super) auto_assigned_revisions: BTreeMap<String, u64>,
    pub(super) tray: Option<TrayHandle>,
    _tray_pump: Option<Task<()>>,
    _pump: Task<()>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("pod_mode", &self.pod_mode)
            .field("settings_section", &self.settings_section)
            .field("pod_visible", &self.pod_visible)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// Builds the UI state hub and starts the session/tray event pumps.
    pub fn new(cx: &mut Context<Self>, init: HubInit) -> Self {
        let mut updates = init.updates;
        let pump = cx.spawn(async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this.update(cx, |this, cx| this.apply(update, cx)).is_err() {
                    break;
                }
            }
        });
        let tray_pump = init.tray_rx.map(|mut actions| {
            cx.spawn(async move |this, cx| {
                while let Some(action) = actions.recv().await {
                    if this
                        .update(cx, |this, cx| this.handle_tray_action(action, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        Self {
            model: Model::new(init.discovery),
            options: init.options,
            rpc: RpcDispatcher::new(init.connection, init.runtime, init.updates_tx),
            commands: init.commands,
            pod_mode: PodMode::MiniVoice,
            settings_section: SettingsSection::Companions,
            settings_window: None,
            pod_window: None,
            pod_view: None,
            pod_visible: true,
            pod_scroll_ping: 0,
            cleared_secret_for: None,
            created_ping: 0,
            setup_pending: false,
            setup_error: None,
            setup_error_detail: None,
            pending_setup_connection_id: None,
            pending_setup_models: Vec::new(),
            checking_connections: HashSet::new(),
            auto_assigned_revisions: BTreeMap::new(),
            tray: init.tray,
            _tray_pump: tray_pump,
            _pump: pump,
        }
    }

    pub(crate) fn theme(&self) -> theme::Theme {
        theme::Theme::strawberry_milk()
    }

    pub(crate) fn has_audio_connection(
        &self,
        kind: ene_client_core::domain::ConnectionKind,
    ) -> bool {
        self.model
            .settings()
            .is_some_and(|s| s.connections.iter().any(|c| c.enabled && c.kind == kind))
    }

    pub(crate) fn live(&self) -> bool {
        self.model.connections.link.is_live() && self.rpc.host().is_some()
    }

    fn apply(&mut self, mut update: Update, cx: &mut Context<Self>) {
        if let Update::Scoped {
            epoch,
            seq: _,
            inner,
        } = update
        {
            if epoch != self.model.connections.link_epoch {
                return;
            }
            update = *inner;
        }
        let flags = self.pre_apply_hub_state(&update, cx);
        let had_companion = self.model.companion_id().is_some();
        let had_approvals = self.model.approvals().len();
        let effects = self.model.apply(update);
        self.dispatch_apply_effects(flags, effects, had_companion, had_approvals, cx);
    }
}

/// Configuration for fetching or initializing a draft input in [`window_draft`].
pub(crate) struct DraftSpec<'a> {
    pub key: String,
    pub placeholder: &'static str,
    pub initial: Option<&'a str>,
    pub look: crate::input::FieldLook,
    pub context: &'static str,
}

/// Keeps a per-window draft entity map synced with the active field theme.
pub(crate) fn window_draft(
    drafts: &mut BTreeMap<String, gpui::Entity<crate::input::Draft>>,
    cx: &mut gpui::App,
    spec: DraftSpec<'_>,
) -> gpui::Entity<crate::input::Draft> {
    let entity = drafts
        .entry(spec.key)
        .or_insert_with(|| {
            cx.new(|cx| {
                let mut draft = crate::input::Draft::new(cx, spec.placeholder, spec.context);
                if let Some(value) = spec.initial {
                    draft.set(value.to_owned(), cx);
                }
                draft
            })
        })
        .clone();
    if entity.read(cx).look() != &spec.look {
        entity.update(cx, |draft, _| draft.apply_look(spec.look));
    }
    entity
}
