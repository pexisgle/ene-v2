//! Window lifecycle, tray state synchronization, and floating pod management for [`Hub`].

use super::{
    CompanionPod, Hub, PodMode, SettingsSection, SettingsStudio, focus_chat,
    initial_pod_bounds_for_mode, pod,
};
use crate::{
    state::{Device, Link, NoticeKind},
    tray::{TrayAction, TrayHandle, TrayState},
};
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, WeakEntity, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, px, size,
};

impl Hub {
    pub fn is_tray_resident(&self) -> bool {
        self.tray.as_ref().is_some_and(TrayHandle::is_resident)
    }

    pub fn is_pod_visible(&self) -> bool {
        self.pod_visible && self.pod_window.is_some()
    }

    pub(crate) fn pod_view(&self) -> Option<WeakEntity<CompanionPod>> {
        self.pod_view.clone()
    }

    pub(crate) fn register_pod_window(
        &mut self,
        handle: WindowHandle<CompanionPod>,
        view: WeakEntity<CompanionPod>,
        cx: &mut Context<Self>,
    ) {
        self.pod_window = Some(handle);
        self.pod_view = Some(view);
        self.pod_visible = true;
        self.sync_tray_state();
        cx.notify();
    }

    pub fn hide_pod(&mut self, cx: &mut Context<Self>) {
        self.pod_visible = false;
        pod::set_pod_window_visible(false);
        self.sync_tray_state();
        cx.notify();
    }

    pub fn open_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        if let Some(next) = mode {
            self.pod_mode = next;
            if next == PodMode::ExpandedChat {
                self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
            }
        }
        self.pod_visible = true;
        let existing = self.pod_window;
        self.sync_tray_state();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _ = cx.update(|cx| {
                let Some(hub) = this.upgrade() else {
                    return;
                };
                if let Some(handle) = existing
                    && handle
                        .update(cx, |view, window, cx| {
                            pod::set_pod_window_visible(true);
                            view.sync_mode_and_focus(window, cx);
                            window.activate_window();
                            cx.activate(true);
                            cx.notify();
                        })
                        .is_ok()
                {
                    return;
                }
                if open_pod_window(cx, hub).is_some() {
                    cx.activate(true);
                }
            });
        })
        .detach();
    }

    pub fn toggle_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        if self.is_pod_visible() && mode.is_none_or(|m| m == self.pod_mode) {
            self.hide_pod(cx);
        } else {
            self.open_pod(mode, cx);
        }
    }

    pub fn sync_window_closed(&mut self, cx: &mut App) {
        let windows = cx.windows();
        if self
            .settings_window
            .is_some_and(|handle| !windows.contains(&handle.into()))
        {
            self.settings_window = None;
        }
        if self
            .pod_window
            .is_some_and(|handle| !windows.contains(&handle.into()))
        {
            self.pod_window = None;
            self.pod_view = None;
            self.pod_visible = false;
        }
        self.sync_tray_state();
        if windows.is_empty() || (!self.is_tray_resident() && self.pod_window.is_none()) {
            cx.quit();
        }
    }

    pub(super) fn sync_tray_state(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let name = self
            .model
            .companion()
            .map(|c| c.name.as_str())
            .unwrap_or("ene");
        let status = match &self.model.connections.link {
            Link::Live { .. } if !self.model.approvals().is_empty() => "確認待ち",
            Link::Live { .. } if self.model.companions.work_active => "作業中",
            Link::Live { .. } if self.model.audio.companion_speaking => "発話中",
            Link::Live { .. } if !self.model.companions.streaming.is_empty() => "生成中",
            Link::Live { .. } => "待機中",
            Link::Connecting { .. } => "接続中",
            _ => "未接続",
        };
        tray.update(TrayState {
            pod_open: self.is_pod_visible(),
            pod_mode: self.pod_mode,
            mic_muted: self.model.audio.mic_muted,
            avatar_ready: matches!(self.model.connections.devices.avatar, Device::Ready(_)),
            tooltip: format!("{name} — {status}"),
        });
    }

    pub(super) fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::ActivatePod => self.open_pod(None, cx),
            TrayAction::OpenChat => self.open_pod(Some(PodMode::ExpandedChat), cx),
            TrayAction::OpenMiniPod => self.open_pod(Some(PodMode::MiniVoice), cx),
            TrayAction::HidePod => self.hide_pod(cx),
            TrayAction::OpenSettings(section) => self.open_settings(section, cx),
            TrayAction::CenterAvatar => self.center_avatar(),
            TrayAction::ToggleMicMute => self.toggle_mic_mute(cx),
            TrayAction::Quit => cx.quit(),
        }
    }

    pub(crate) fn set_pod_mode(&mut self, mode: PodMode, cx: &mut Context<Self>) {
        if self.pod_mode != mode {
            self.pod_mode = mode;
            if mode == PodMode::ExpandedChat {
                self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
            }
            self.sync_tray_state();
            cx.notify();
        }
    }

    /// Opens or focuses the standalone `SettingsStudio` window at `section`.
    pub fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.settings_section = section;
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
        self.load_companion_templates(cx);
        self.load_policy_rules(cx);
        self.load_plugins(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _ = cx.update(|cx| {
                let Some(hub) = this.upgrade() else {
                    return;
                };
                let existing = hub.read(cx).settings_window;
                if let Some(handle) = existing
                    && handle
                        .update(cx, |_, window, cx| {
                            window.activate_window();
                            cx.notify();
                        })
                        .is_ok()
                {
                    return;
                }

                let hub_for_view = hub.clone();
                let bounds = settings_window_bounds(cx);
                let opened = cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(680.), px(480.))),
                        window_decorations: Some(gpui::WindowDecorations::Client),
                        app_id: Some("ene-desktop".into()),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("設定 — ene".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    move |_window, cx| cx.new(|cx| SettingsStudio::new(hub_for_view.clone(), cx)),
                );
                match opened {
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                        let _ = this.update(cx, |hub, cx| {
                            hub.settings_window = Some(handle);
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        let _ = this.update(cx, |hub, cx| {
                            hub.model.notice(
                                NoticeKind::Failed,
                                "設定ウィンドウを開く",
                                error.to_string(),
                            );
                            cx.notify();
                        });
                    }
                }
            });
        })
        .detach();
    }
}

fn settings_window_bounds(cx: &gpui::App) -> Bounds<gpui::Pixels> {
    let default_size = size(px(820.), px(720.));
    if let Some(display) = cx.primary_display() {
        let visible = display.bounds();
        let width = px(820.).min((visible.size.width - px(96.)).max(px(680.)));
        let height = px(740.).min((visible.size.height - px(96.)).max(px(480.)));
        // Place the settings window on the left half when wide enough so it
        // does not collide with the centered desktop avatar.
        if visible.size.width >= px(1240.) {
            return Bounds::new(
                gpui::point(visible.origin.x + px(48.), visible.origin.y + px(44.)),
                size(width, height),
            );
        }
        return Bounds::centered(Some(display.id()), size(width, height), cx);
    }
    Bounds::centered(None, default_size, cx)
}

/// Opens the floating companion pod window and registers it with `Hub`.
pub fn open_pod_window(cx: &mut App, hub: Entity<Hub>) -> Option<WindowHandle<CompanionPod>> {
    let mode = hub.read(cx).pod_mode;
    let bounds = initial_pod_bounds_for_mode(mode, cx);
    let hub_for_pod = hub.clone();
    let opened = cx.open_window(
        WindowOptions {
            #[cfg(windows)]
            kind: WindowKind::PopUp,
            #[cfg(not(windows))]
            kind: WindowKind::Floating,
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            window_decorations: Some(gpui::WindowDecorations::Client),
            app_id: Some("ene-desktop".into()),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(PodMode::MiniVoice.min_size()),
            is_movable: true,
            is_resizable: false,
            titlebar: None,
            ..Default::default()
        },
        move |_window, cx| cx.new(|cx| CompanionPod::new(hub_for_pod.clone(), cx)),
    );
    match opened {
        Ok(window) => {
            let hub_close = hub.clone();
            if let Ok(view) = window.update(cx, |view, window, cx| {
                focus_chat(view, window, cx);
                window.on_window_should_close(cx, move |_window, cx| {
                    let should_hide = hub_close.read(cx).is_tray_resident()
                        || hub_close.read(cx).settings_window.is_some();
                    if should_hide {
                        hub_close.update(cx, |hub, cx| hub.hide_pod(cx));
                        false
                    } else {
                        true
                    }
                });
                cx.entity().downgrade()
            }) {
                hub.update(cx, |hub, cx| hub.register_pod_window(window, view, cx));
            }
            Some(window)
        }
        Err(error) => {
            eprintln!("ene-desktop: the window did not open: {error:#}");
            None
        }
    }
}
