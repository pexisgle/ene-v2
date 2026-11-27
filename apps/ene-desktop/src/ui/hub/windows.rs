//! Window lifecycle, tray state synchronization, and floating pod management for [`Hub`].

use super::Hub;
use crate::{
    state::{Device, Link, NoticeKind},
    ui::{
        CompanionPod, PodMode, SettingsSection, SettingsStudio, focus_chat,
        initial_pod_bounds_for_mode, pod,
    },
};
use ene_platform_os::{TrayAction, TrayHandle, TrayMode, TraySection, TrayState};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, WeakEntity, WindowBounds,
    WindowKind, WindowOptions, px, size,
};

pub(super) fn is_tray_resident(hub: &Hub) -> bool {
    hub.windows
        .tray
        .as_ref()
        .is_some_and(TrayHandle::is_resident)
}

pub(super) fn is_pod_visible(hub: &Hub) -> bool {
    hub.windows.pod_visible && hub.windows.pod_window.is_some()
}

pub(super) fn pod_view(hub: &Hub) -> Option<WeakEntity<CompanionPod>> {
    hub.windows.pod_view.clone()
}

pub(super) fn register_pod_window(
    hub: &mut Hub,
    handle: AnyWindowHandle,
    view: WeakEntity<CompanionPod>,
    cx: &mut Context<Hub>,
) {
    hub.windows.pod_window = Some(handle);
    hub.windows.pod_view = Some(view);
    hub.windows.pod_visible = true;
    sync_tray_state(hub);
    cx.notify();
}

pub(super) fn hide_pod(hub: &mut Hub, cx: &mut Context<Hub>) {
    hub.windows.pod_visible = false;
    pod::set_pod_window_visible(false);
    sync_tray_state(hub);
    cx.notify();
}

pub(super) fn open_pod(hub: &mut Hub, mode: Option<PodMode>, cx: &mut Context<Hub>) {
    if let Some(next) = mode {
        hub.windows.pod_mode = next;
        if next == PodMode::ExpandedChat {
            hub.windows.pod_scroll_ping = hub.windows.pod_scroll_ping.saturating_add(1);
        }
    }
    hub.windows.pod_visible = true;
    let existing = hub.windows.pod_window.zip(hub.pod_view());
    sync_tray_state(hub);
    cx.notify();

    cx.spawn(async move |this, cx| {
        cx.update(|cx| {
            let Some(hub) = this.upgrade() else {
                return;
            };
            if let Some((handle, pod_view)) = existing
                && handle
                    .update(cx, |_, window, cx| {
                        pod::set_pod_window_visible(true);
                        let _updated = pod_view.update(cx, |view, cx| {
                            view.sync_mode_and_focus(window, cx);
                        });
                        window.activate_window();
                        cx.activate(true);
                    })
                    .is_ok()
            {
                return;
            }
            if open_pod_window(cx, &hub).is_some() {
                cx.activate(true);
            }
        });
    })
    .detach();
}

pub(super) fn toggle_pod(hub: &mut Hub, mode: Option<PodMode>, cx: &mut Context<Hub>) {
    if is_pod_visible(hub) && mode.is_none_or(|m| m == hub.windows.pod_mode) {
        hide_pod(hub, cx);
    } else {
        open_pod(hub, mode, cx);
    }
}

pub(super) fn sync_window_closed(hub: &mut Hub, cx: &mut App) {
    let windows = cx.windows();
    if hub
        .windows
        .settings_window
        .is_some_and(|handle| !windows.contains(&handle))
    {
        hub.windows.settings_window = None;
    }
    if hub
        .windows
        .pod_window
        .is_some_and(|handle| !windows.contains(&handle))
    {
        hub.windows.pod_window = None;
        hub.windows.pod_view = None;
        hub.windows.pod_visible = false;
    }
    sync_tray_state(hub);
    if windows.is_empty() || (!is_tray_resident(hub) && hub.windows.pod_window.is_none()) {
        cx.quit();
    }
}

pub(super) fn sync_tray_state(hub: &Hub) {
    let Some(tray) = &hub.windows.tray else {
        return;
    };
    let name = hub.model.companion().map_or("ene", |c| c.name.as_str());
    let status = match &hub.model.connections.link {
        Link::Live { .. } if !hub.model.approvals().is_empty() => "確認待ち",
        Link::Live { .. } if hub.model.has_active_work() => "作業中",
        Link::Live { .. } if hub.model.audio.companion_speaking => "発話中",
        Link::Live { .. } if !hub.model.companions.streaming.is_empty() => "生成中",
        Link::Live { .. } => "待機中",
        Link::Connecting { .. } => "接続中",
        _ => "未接続",
    };
    tray.update(TrayState {
        pod_open: is_pod_visible(hub),
        pod_mode: tray_mode(hub.windows.pod_mode),
        mic_muted: hub.model.audio.mic_muted,
        avatar_ready: matches!(hub.model.connections.devices.avatar, Device::Ready(_)),
        tooltip: format!("{name} — {status}"),
    });
}

/// The tray crate's copy of the pod mode, which it cannot import from `ui`.
fn tray_mode(mode: PodMode) -> TrayMode {
    match mode {
        PodMode::MiniVoice => TrayMode::MiniVoice,
        PodMode::ExpandedChat => TrayMode::ExpandedChat,
    }
}

/// The settings chapter the tray asked for, in the UI's own type.
fn settings_section(section: TraySection) -> SettingsSection {
    match section {
        TraySection::Companions => SettingsSection::Companions,
        TraySection::Connections => SettingsSection::Connections,
        TraySection::Voice => SettingsSection::Voice,
        TraySection::Device => SettingsSection::Device,
    }
}

pub(super) fn handle_tray_action(hub: &mut Hub, action: TrayAction, cx: &mut Context<Hub>) {
    match action {
        TrayAction::ActivatePod => open_pod(hub, None, cx),
        TrayAction::OpenChat => open_pod(hub, Some(PodMode::ExpandedChat), cx),
        TrayAction::OpenMiniPod => open_pod(hub, Some(PodMode::MiniVoice), cx),
        TrayAction::HidePod => hide_pod(hub, cx),
        TrayAction::OpenSettings(section) => open_settings(hub, settings_section(section), cx),
        TrayAction::CenterAvatar => hub.center_avatar(),
        TrayAction::ToggleMicMute => hub.toggle_mic_mute(cx),
        TrayAction::Quit => cx.quit(),
    }
}

pub(super) fn set_pod_mode(hub: &mut Hub, mode: PodMode, cx: &mut Context<Hub>) {
    if hub.windows.pod_mode != mode {
        hub.windows.pod_mode = mode;
        if mode == PodMode::ExpandedChat {
            hub.windows.pod_scroll_ping = hub.windows.pod_scroll_ping.saturating_add(1);
        }
        sync_tray_state(hub);
        cx.notify();
    }
}

pub(super) fn open_settings(hub: &mut Hub, section: SettingsSection, cx: &mut Context<Hub>) {
    hub.windows.settings_section = section;
    hub.load_providers(cx);
    hub.load_models(cx);
    hub.load_mind(cx);
    hub.load_companion_templates(cx);
    hub.load_policy_rules(cx);
    hub.load_plugins(cx);
    cx.notify();

    cx.spawn(async move |this, cx| {
        cx.update(|cx| {
            let Some(hub) = this.upgrade() else {
                return;
            };
            let existing = hub.read(cx).windows.settings_window;
            if let Some(handle) = existing
                && handle
                    .update(cx, |_, window, _| window.activate_window())
                    .is_ok()
            {
                return;
            }

            let hub_for_view = hub.clone();
            let bounds = settings_window_bounds(cx);
            let opened = gpui::open_window(
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
                cx,
                move |_window, cx| cx.new(|cx| SettingsStudio::new(hub_for_view, cx)),
            );
            match opened {
                Ok((handle, _)) => {
                    ene_domain::debug_if_err(
                        handle.update(cx, |_, window, _| window.activate_window()),
                        "window activation",
                    );
                    let _activated = this.update(cx, |hub, cx| {
                        hub.windows.settings_window = Some(handle);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _activated = this.update(cx, |hub, cx| {
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
pub fn open_pod_window(cx: &mut App, hub: &Entity<Hub>) -> Option<AnyWindowHandle> {
    let mode = hub.read(cx).windows.pod_mode;
    let bounds = initial_pod_bounds_for_mode(mode, cx);
    let hub_for_pod = hub.clone();
    let opened = gpui::open_window(
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
        cx,
        move |window, cx| cx.new(|cx| CompanionPod::new(hub_for_pod, window, cx)),
    );
    match opened {
        Ok((window, view)) => {
            let hub_close = hub.clone();
            let did_open = window.update(cx, |_, window, cx| {
                view.update(cx, |view, cx| focus_chat(view, window, cx));
                window.on_window_should_close(cx, move |_window, cx| {
                    let should_hide = hub_close.read(cx).is_tray_resident()
                        || hub_close.read(cx).windows.settings_window.is_some();
                    if should_hide {
                        hub_close.update(cx, super::Hub::hide_pod);
                        false
                    } else {
                        true
                    }
                });
            });
            if did_open.is_ok() {
                hub.update(cx, |hub, cx| {
                    hub.register_pod_window(window, view.downgrade(), cx)
                });
            }
            Some(window)
        }
        Err(error) => {
            tracing::error!(error = ?error, "failed to open window");
            None
        }
    }
}
