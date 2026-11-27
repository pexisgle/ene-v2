//! State types owned by [`Hub`]: window/tray handles, the setup flow, and the
//! per-update flags the hub computes before handing an update to the reducer.

use super::{PodMode, SettingsSection};
use crate::session::{Connection, Options};
use crate::ui::CompanionPod;
use crate::update::{Command, Update};
use ene_client_core::domain::Connection as ProviderConnection;
use ene_client_core::domain::NotificationEvent;
use ene_client_core::state::{Notice, NoticeKind};
use ene_platform_os::{TrayAction, TrayHandle};
use gpui::{AnyWindowHandle, WeakEntity};
use std::collections::HashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Window handles, floating pod mode, and tray state owned by [`Hub`].
#[derive(Default)]
pub struct WindowState {
    pub pod_mode: PodMode,
    pub settings_section: SettingsSection,
    pub settings_window: Option<AnyWindowHandle>,
    pub pod_window: Option<AnyWindowHandle>,
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

/// First-run onboarding and connection check state owned by [`Hub`].
#[derive(Debug, Default)]
pub struct SetupFlowState {
    pub(crate) stage: SetupStage,
    pub(crate) error: Option<Notice>,
    pub checking_connections: HashMap<String, String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum SetupStage {
    #[default]
    Idle,
    SavingConnection,
    Choosing(String),
    Applying(String),
}

impl SetupFlowState {
    pub(crate) fn connection_id(&self) -> Option<&str> {
        match &self.stage {
            SetupStage::Choosing(id) | SetupStage::Applying(id) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn can_apply(&self, connection: &ProviderConnection, model_id: &str) -> bool {
        connection.enabled
            && self.connection_id().is_none_or(|id| {
                id == connection.id
                    && connection.last_check.as_ref().is_some_and(|check| {
                        check.healthy && check.checked_model.as_deref() == Some(model_id)
                    })
            })
    }

    pub(super) fn disconnect(&mut self) {
        self.checking_connections.clear();
        self.stage = match std::mem::take(&mut self.stage) {
            SetupStage::SavingConnection => SetupStage::Idle,
            SetupStage::Applying(id) => SetupStage::Choosing(id),
            other => {
                self.stage = other;
                return;
            }
        };
        self.error = Some(Notice {
            context: "接続が切れました".into(),
            message: "再接続後に保存を確認してください。".into(),
            kind: NoticeKind::Failed,
        });
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

pub(super) struct ApplyFlags {
    pub(super) conflicted: bool,
    pub(super) history_arrived: bool,
    pub(super) settings_changed: bool,
    pub(super) message_arrived: bool,
}

impl ApplyFlags {
    pub(super) fn from_update(update: &Update) -> Self {
        let notification_event = match update {
            Update::Notification(notification) => notification.event.as_ref(),
            _ => None,
        };
        Self {
            conflicted: matches!(
                update,
                Update::Notice(notice)
                    | Update::Checked {
                        result: Err(notice),
                        ..
                    } if notice.kind == NoticeKind::Conflict
            ),
            history_arrived: matches!(update, Update::History { .. }),
            settings_changed: matches!(
                notification_event,
                Some(NotificationEvent::SettingsUpdated(_))
            ),
            message_arrived: matches!(
                notification_event,
                Some(NotificationEvent::MessageCreated(_) | NotificationEvent::MessageDelta(_))
            ),
        }
    }
}
