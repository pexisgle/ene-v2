//! State types owned by [`Hub`]: window/tray handles, the setup flow, and the
//! per-update flags the hub computes before handing an update to the reducer.

use super::{CompanionPod, PodMode, SettingsSection};
use crate::session::{Connection, Options};
use crate::ui::SettingsStudio;
use crate::update::{Command, Update};
use ene_client_core::domain::NotificationEvent;
use ene_platform_os::{TrayAction, TrayHandle};
use gpui::{WeakEntity, WindowHandle};
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
    pub(super) fn from_update(update: &Update) -> Self {
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
