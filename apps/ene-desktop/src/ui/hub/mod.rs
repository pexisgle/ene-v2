//! Shared UI state hub, lifecycle state grouping, and host update dispatch.

mod actions;
mod companions;
mod connections;
mod presets;
mod windows;

pub use windows::open_pod_window;

use super::{CompanionPod, PodMode, SettingsSection, SettingsStudio};
use crate::{
    rpc::{self, RpcDispatcher},
    session::{Connection, Options},
    state::{Effects, Model},
    update::{Command, Update},
};
use ene_client_core::domain::{ConnectionKind, NotificationEvent};
use ene_platform_os::{TrayAction, TrayHandle};
use gpui::{App, Context, Task, WeakEntity, WindowHandle};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Window handles, floating pod mode, and tray state owned by [`Hub`].
#[derive(Default)]
pub struct WindowState {
    pub pod_mode: PodMode,
    pub settings_section: SettingsSection,
    pub settings_window: Option<WindowHandle<SettingsStudio>>,
    pub pod_window: Option<WindowHandle<CompanionPod>>,
    pub(crate) pod_view: Option<WeakEntity<CompanionPod>>,
    pub(crate) pod_visible: bool,
    pub pod_scroll_ping: u64,
    pub(crate) tray: Option<TrayHandle>,
}

impl std::fmt::Debug for WindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowState")
            .field("pod_mode", &self.pod_mode)
            .field("settings_section", &self.settings_section)
            .field("pod_visible", &self.pod_visible)
            .finish_non_exhaustive()
    }
}

/// First-run onboarding, connection check, and secret-clear tracking owned by [`Hub`].
#[derive(Debug, Default)]
pub struct SetupFlowState {
    pub cleared_secret_for: Option<(String, u64)>,
    pub created_ping: u64,
    pub(crate) pending: bool,
    pub(crate) error: Option<&'static str>,
    pub error_detail: Option<String>,
    pub pending_connection_id: Option<String>,
    pub pending_models: Vec<String>,
    pub checking_connections: HashSet<String>,
    pub(crate) auto_assigned_revisions: BTreeMap<String, u64>,
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

pub(super) struct ApplyFlags {
    pub(super) conflicted: bool,
    pub(super) history_arrived: bool,
    pub(super) settings_changed: bool,
    pub(super) companion_or_snapshot: bool,
    pub(super) models_or_check: bool,
    pub(super) message_arrived: bool,
}

impl ApplyFlags {
    fn from_update(update: &Update) -> Self {
        let notification_event = match update {
            Update::Notification(notification) => notification.event.as_ref(),
            _ => None,
        };
        Self {
            conflicted: matches!(update, Update::Conflict { .. }),
            history_arrived: matches!(update, Update::History { .. }),
            settings_changed: matches!(
                notification_event,
                Some(NotificationEvent::SettingsUpdated(_))
            ),
            companion_or_snapshot: matches!(
                notification_event,
                Some(
                    NotificationEvent::InitialState(_)
                        | NotificationEvent::Companion(_)
                        | NotificationEvent::SettingsUpdated(_)
                )
            ),
            models_or_check: matches!(
                update,
                Update::Providers(_) | Update::Models { .. } | Update::Checked { .. }
            ),
            message_arrived: matches!(
                notification_event,
                Some(NotificationEvent::MessageCreated(_) | NotificationEvent::MessageDelta(_))
            ),
        }
    }
}

/// Single shared state hub observed by `CompanionPod`, `SettingsStudio`, and its panes.
pub struct Hub {
    pub model: Model,
    pub options: Options,
    pub(super) rpc: RpcDispatcher,
    pub(super) commands: UnboundedSender<Command>,
    pub windows: WindowState,
    pub setup: SetupFlowState,
    _tray_pump: Option<Task<()>>,
    _pump: Task<()>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("windows", &self.windows)
            .field("setup", &self.setup)
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
            windows: WindowState {
                pod_visible: true,
                tray: init.tray,
                ..Default::default()
            },
            setup: SetupFlowState::default(),
            _tray_pump: tray_pump,
            _pump: pump,
        }
    }

    pub(crate) fn theme(&self) -> super::theme::Theme {
        super::theme::Theme::strawberry_milk()
    }

    pub(crate) fn has_audio_connection(&self, kind: ConnectionKind) -> bool {
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

    fn pre_apply_hub_state(&mut self, update: &Update, cx: &mut Context<Self>) -> ApplyFlags {
        self.apply_ui_update_bookkeeping(update, cx);
        if let Update::Checked {
            connection_id: _,
            check,
        } = update
            && check.healthy
        {
            self.load_models(cx);
        }
        if let Update::Failed { context, .. }
        | Update::Conflict { context, .. }
        | Update::Denied { context, .. } = update
            && *context == "接続確認"
        {
            self.setup.checking_connections.clear();
        }
        if let Update::SendFailed { reason, .. } = update {
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        ApplyFlags::from_update(update)
    }

    fn apply_ui_update_bookkeeping(&mut self, update: &Update, cx: &mut Context<Self>) {
        match update {
            Update::CredentialRegistered { connection_id } => {
                let next_seq = self
                    .setup
                    .cleared_secret_for
                    .as_ref()
                    .map_or(1, |(_, seq)| seq + 1);
                self.setup.cleared_secret_for = Some((connection_id.clone(), next_seq));
            }
            Update::Checked {
                connection_id: id,
                check,
            } => {
                self.setup.checking_connections.remove(id);
                if check.healthy
                    && self.model.settings().is_some_and(|s| {
                        s.connections
                            .iter()
                            .any(|c| &c.id == id && c.kind == ConnectionKind::Llm)
                    })
                {
                    self.setup.error = None;
                    self.setup.error_detail = None;
                }
            }
            Update::SetupFinished {
                ok,
                connection_id,
                models,
                error,
            } => {
                self.setup.pending = false;
                if *ok {
                    self.setup.error = None;
                    self.setup.error_detail = None;
                    self.setup.pending_connection_id = connection_id.clone();
                    self.setup.pending_models = models.clone();
                } else {
                    self.setup.error = Some(
                        "接続を確認できませんでした。選んだサービスのキーとネットワークを確認して、もう一度登録してください。",
                    );
                    self.setup.error_detail = error.clone();
                }
                self.resnapshot();
                if *ok {
                    self.load_models(cx);
                }
            }
            Update::Resnapshot => self.resnapshot(),
            Update::Created(id) => {
                let created_id = id.clone();
                self.setup.created_ping = self.setup.created_ping.saturating_add(1);
                self.resnapshot();
                self.select_companion(&created_id, cx);
            }
            _ => {}
        }
    }

    fn dispatch_apply_effects(
        &mut self,
        flags: ApplyFlags,
        effects: Effects,
        had_companion: bool,
        had_approvals: usize,
        cx: &mut Context<Self>,
    ) {
        let selected_now = !had_companion && self.model.companion_id().is_some();
        if flags.settings_changed {
            let _discarded = self.commands.send(Command::Refresh);
            self.load_models(cx);
        }
        if flags.companion_or_snapshot || flags.models_or_check || selected_now {
            self.maybe_auto_assign_default_model();
        }
        if flags.conflicted {
            self.resnapshot();
            self.refresh_active_data(cx);
        }
        if self.model.approvals().len() > had_approvals
            || flags.message_arrived
            || flags.history_arrived
        {
            self.windows.pod_scroll_ping = self.windows.pod_scroll_ping.saturating_add(1);
        }
        if effects.history || selected_now {
            self.load_history(cx);
        }
        if effects.approvals || selected_now {
            self.load_approvals(cx);
        }
        if effects.mind || selected_now {
            self.load_mind(cx);
        }
        if self.live() && self.model.connections.providers.is_none() {
            self.load_providers(cx);
        }
        if selected_now {
            self.load_models(cx);
            self.load_policy_rules(cx);
            self.load_policy_texts(cx);
        }
        self.sync_tray_state();
        cx.notify();
    }

    fn maybe_auto_assign_default_model(&mut self) {
        if !self.live() || self.setup.pending || self.setup.pending_connection_id.is_some() {
            return;
        }
        let Some(patch) = self
            .model
            .auto_assign_default_model_request(&self.setup.auto_assigned_revisions)
        else {
            return;
        };
        if self.rpc.host().is_none() {
            return;
        }
        self.setup
            .auto_assigned_revisions
            .insert(patch.companion_id.clone(), patch.revision);
        rpc::companions::auto_assign_default_model(
            &self.rpc,
            self.model.connections.link_epoch,
            patch,
        );
    }

    pub(crate) fn refresh_active_data(&mut self, cx: &mut Context<Self>) {
        self.load_history(cx);
        self.load_approvals(cx);
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
        self.load_policy_rules(cx);
        self.load_policy_texts(cx);
    }

    pub(crate) fn resnapshot(&self) {
        rpc::chat::resnapshot(&self.rpc, self.model.connections.link_epoch);
    }

    pub fn is_tray_resident(&self) -> bool {
        windows::is_tray_resident(self)
    }

    pub fn is_pod_visible(&self) -> bool {
        windows::is_pod_visible(self)
    }

    pub(crate) fn pod_view(&self) -> Option<WeakEntity<CompanionPod>> {
        windows::pod_view(self)
    }

    pub(crate) fn register_pod_window(
        &mut self,
        handle: WindowHandle<CompanionPod>,
        view: WeakEntity<CompanionPod>,
        cx: &mut Context<Self>,
    ) {
        windows::register_pod_window(self, handle, view, cx);
    }

    pub fn hide_pod(&mut self, cx: &mut Context<Self>) {
        windows::hide_pod(self, cx);
    }

    pub fn open_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        windows::open_pod(self, mode, cx);
    }

    pub fn toggle_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        windows::toggle_pod(self, mode, cx);
    }

    pub fn sync_window_closed(&mut self, cx: &mut App) {
        windows::sync_window_closed(self, cx);
    }

    pub(crate) fn sync_tray_state(&self) {
        windows::sync_tray_state(self);
    }

    pub(crate) fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        windows::handle_tray_action(self, action, cx);
    }

    pub(crate) fn set_pod_mode(&mut self, mode: PodMode, cx: &mut Context<Self>) {
        windows::set_pod_mode(self, mode, cx);
    }

    /// Opens or focuses the standalone `SettingsStudio` window at `section`.
    pub fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        windows::open_settings(self, section, cx);
    }

    pub(crate) fn tts_voice_choices(&self) -> Vec<(String, String, String, String)> {
        self.model.tts_voice_choices()
    }

    pub(crate) fn voice_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.voice_choices_for_connection(conn_id)
    }

    pub(crate) fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.stt_model_choices_for_connection(conn_id)
    }

    pub(crate) fn embedding_model_choices_for_connection(
        &self,
        conn_id: &str,
    ) -> Vec<(String, String)> {
        self.model.embedding_model_choices_for_connection(conn_id)
    }

    pub(crate) fn available_models_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.available_models_for_connection(conn_id)
    }

    pub(crate) fn model_choices(&self) -> Vec<(String, String, String)> {
        self.model.model_choices()
    }

    pub(crate) fn all_llm_models(&self) -> Vec<(String, String, String)> {
        self.model.all_llm_models()
    }

    pub(crate) fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.model.all_embedding_models()
    }

    pub(crate) fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.model.all_stt_models()
    }

    pub(crate) fn active_stt_selection(&self) -> Option<(String, String)> {
        self.model.active_stt_selection()
    }

    pub(crate) fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.model.all_tts_voices()
    }

    pub(crate) fn active_tts_selection(&self) -> Option<(String, String)> {
        self.model.active_tts_selection()
    }
}
