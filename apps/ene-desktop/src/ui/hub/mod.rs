//! Shared UI state hub, lifecycle state grouping, and host update dispatch.

mod actions;
mod chat;
mod companions;
mod connections;
pub(crate) mod presets;
mod state;
pub(crate) mod voice_roles;
mod windows;

pub(crate) use state::SetupStage;
pub use state::{HubInit, SetupFlowState, WindowState};
pub use windows::open_pod_window;

use super::{CompanionPod, PodMode, SettingsSection};
use crate::{
    rpc::{self, RpcDispatcher},
    session::Options,
    state::{Effects, Model},
    update::{Command, Update},
};
use ene_client_core::domain::ConnectionKind;
use ene_platform_os::TrayAction;
use gpui::{AnyWindowHandle, App, Context, EventEmitter, Task, WeakEntity};
use state::ApplyFlags;
use tokio::sync::mpsc::UnboundedSender;

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

pub(super) enum HubEvent {
    CredentialsSaved(String),
    CompanionCreated(String),
    CompanionPreviewFinished,
}

impl EventEmitter<HubEvent> for Hub {}

#[cfg(test)]
mod tests;

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

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        let Some(update) = self.model.accept_update(update) else {
            return;
        };
        if let Update::Checked {
            connection_id,
            ticket,
            ..
        } = &update
            && self.setup.checking_connections.get(connection_id) != Some(ticket)
        {
            return;
        }
        let flags = self.pre_apply_hub_state(&update, cx);
        let had_companion = self.model.companion_id().is_some();
        let had_approvals = self.model.approvals().len();
        let effects = self.model.apply_accepted(update);
        self.dispatch_apply_effects(&flags, effects, had_companion, had_approvals, cx);
    }

    fn pre_apply_hub_state(&mut self, update: &Update, cx: &mut Context<Self>) -> ApplyFlags {
        self.apply_ui_update_bookkeeping(update, cx);
        if let Update::Checked {
            result: Ok(true), ..
        } = update
        {
            self.load_models(cx);
        }
        if let Update::SendFailed { reason, .. } = update {
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        ApplyFlags::from_update(update)
    }

    fn apply_ui_update_bookkeeping(&mut self, update: &Update, cx: &mut Context<Self>) {
        match update {
            Update::Link(link) if !link.is_live() => {
                self.setup.disconnect();
                cx.emit(HubEvent::CompanionPreviewFinished);
            }
            Update::CredentialRegistered { connection_id } => {
                cx.emit(HubEvent::CredentialsSaved(connection_id.clone()));
            }
            Update::Checked { connection_id, .. } => {
                self.setup.checking_connections.remove(connection_id);
            }
            Update::SetupSaved(result) if self.setup.stage == SetupStage::SavingConnection => {
                self.setup.stage = match result {
                    Ok(id) => SetupStage::Choosing(id.clone()),
                    Err(_) => SetupStage::Idle,
                };
                self.setup.error = result.as_ref().err().cloned();
                self.resnapshot();
            }
            Update::SetupApplied(result) => {
                let SetupStage::Applying(id) = &self.setup.stage else {
                    return;
                };
                self.setup.stage = if result.is_ok() {
                    SetupStage::Idle
                } else {
                    SetupStage::Choosing(id.clone())
                };
                self.setup.error = result.as_ref().err().cloned();
                self.resnapshot();
                if let Ok(id) = result {
                    self.select_companion(id, cx);
                }
            }
            Update::Created(id) => {
                self.resnapshot();
                self.select_companion(id, cx);
                cx.emit(HubEvent::CompanionCreated(id.clone()));
            }
            Update::CompanionPreview { .. } => cx.emit(HubEvent::CompanionPreviewFinished),
            _ => {}
        }
    }

    fn dispatch_apply_effects(
        &mut self,
        flags: &ApplyFlags,
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
        if flags.conflicted {
            self.refresh_active_data(cx);
        } else if effects.snapshot {
            self.resnapshot();
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
        if effects.mind || selected_now {
            self.load_mind(cx);
        }
        if self.live() && self.model.connections.providers.is_none() {
            self.load_providers(cx);
        }
        if selected_now {
            self.load_models(cx);
        }
        self.sync_tray_state();
        cx.notify();
    }

    pub(crate) fn refresh_active_data(&mut self, cx: &mut Context<Self>) {
        self.resnapshot();
        self.load_history(cx);
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
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
        handle: AnyWindowHandle,
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

    pub(crate) fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.model.all_tts_voices()
    }

    pub(crate) fn active_tts_selection(&self) -> Option<(String, String)> {
        self.model.active_tts_selection()
    }
}
