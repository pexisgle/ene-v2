//! Hardware, audio, perception, pod window, avatar, and capability status card for `この端末`.

use super::{DevicePane, DeviceSnapshot, device_pill, row_frame};
use crate::{
    state::{Device, Link},
    ui::{
        icons::Icon,
        theme::Theme,
        widgets::{ButtonStyle, Tone, kind_pill, pill_button, status_pill, toggle_switch},
    },
};
use gpui::{AnyElement, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_device_status_card(
    pane: &DevicePane,
    snap: &DeviceSnapshot,
    theme: &Theme,
) -> AnyElement {
    let card = div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .flex()
        .flex_col()
        .child(render_active_client_row(pane, snap, theme))
        .child(render_mic_row(pane, snap, theme))
        .child(render_output_row(pane, snap, theme))
        .child(render_eyes_row(pane, snap, theme))
        .child(eyes_description_note(theme));

    // `card` is shadowed rather than reassigned: the Windows-only rows append
    // through a new binding, so no platform has an unused `mut`.
    #[cfg(windows)]
    let card = card
        .child(render_computer_use_row(pane, snap, theme))
        .child(computer_use_note(theme));

    card.child(render_pod_window_row(pane, snap, theme))
        .child(render_avatar_controls_row(pane, snap, theme))
        .child(render_capabilities_row(&snap.client_caps, theme))
        .into_any_element()
}

fn render_active_client_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let (link_text, link_dot, link_bg, link_fg) = match &snap.link {
        Link::Live { version } => (
            format!("接続中 (v{version})"),
            theme.mint,
            theme.mint_soft,
            theme.mint_fg,
        ),
        Link::Connecting { .. } => (
            "接続中…".to_owned(),
            theme.amber,
            theme.amber_soft,
            theme.amber_fg,
        ),
        _ => (
            "未接続".to_owned(),
            theme.danger,
            theme.danger_soft,
            theme.danger_fg,
        ),
    };
    let hub_activate = pane.hub.clone();
    row_frame(
        false,
        "対話の担当端末",
        Some(status_pill(link_text, Some(link_dot), link_bg, link_fg)),
        if snap.is_active_client {
            status_pill(
                "この端末で対話中",
                Some(theme.mint),
                theme.mint_soft,
                theme.mint_fg,
            )
        } else {
            pill_button(
                "dev-activate-this",
                "この端末に切り替える",
                Tone::Primary
                    .icon(Icon::Check)
                    .enabled(snap.live && snap.has_companion),
                theme,
                move |_, cx| hub_activate.update(cx, super::super::hub::Hub::activate_this_client),
            )
        },
        theme,
    )
}

fn render_mic_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let mic_ready = snap.mic_configured && matches!(snap.devices.input, Device::Ready(_));
    let hub_mic = pane.hub.clone();
    row_frame(
        true,
        "声で話す",
        Some(if snap.mic_configured {
            device_pill(&snap.devices.input, !snap.mic_muted, theme)
        } else {
            status_pill(
                "音声認識の接続が必要",
                None,
                theme.amber_soft,
                theme.amber_fg,
            )
        }),
        toggle_switch(
            "dev-mic-toggle",
            mic_ready && !snap.mic_muted,
            mic_ready,
            theme,
            move |_, cx| {
                hub_mic.update(cx, super::super::hub::Hub::toggle_mic_mute);
            },
        ),
        theme,
    )
}

fn render_output_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let output_ready = snap.output_configured && matches!(snap.devices.output, Device::Ready(_));
    let hub_out = pane.hub.clone();
    row_frame(
        true,
        "返事を読み上げる",
        Some(if snap.output_configured {
            device_pill(&snap.devices.output, !snap.output_muted, theme)
        } else {
            status_pill(
                "読み上げの接続が必要",
                None,
                theme.amber_soft,
                theme.amber_fg,
            )
        }),
        toggle_switch(
            "dev-out-toggle",
            output_ready && !snap.output_muted,
            output_ready,
            theme,
            move |_, cx| {
                hub_out.update(cx, super::super::hub::Hub::toggle_output_mute);
            },
        ),
        theme,
    )
}

fn render_eyes_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let eyes_supported = crate::perception::supported();
    let eyes_label = match (
        snap.eyes_enabled,
        snap.live && snap.is_active_client,
        snap.eyes_status.as_deref(),
    ) {
        _ if !eyes_supported => "この環境では取得不可",
        (false, _, _) => "オフ",
        (_, false, _) => "担当端末になるまで待機",
        (_, _, Some("unavailable")) => "目のモデルに接続できません",
        (_, _, Some("image")) if cfg!(windows) => "画面と活動を評価",
        (_, _, Some("events" | "image")) => "活動イベントを評価",
        _ => "モデル確認中",
    };
    let hub_eyes = pane.hub.clone();
    let enabled = snap.eyes_enabled;
    row_frame(
        true,
        "目",
        Some(status_pill(
            eyes_label,
            None,
            theme.bg_blush,
            theme.ink_muted,
        )),
        toggle_switch(
            "dev-eyes-toggle",
            enabled,
            eyes_supported || enabled,
            theme,
            move |_, cx| hub_eyes.read(cx).set_eyes_enabled(!enabled),
        ),
        theme,
    )
}

#[cfg(windows)]
fn render_computer_use_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let hub_computer = pane.hub.clone();
    let computer_use = snap.computer_use;
    row_frame(
        true,
        "手に画面操作を許可",
        (snap.live && computer_use && !snap.computer_granted)
            .then(|| status_pill("ホストの許可が必要", None, theme.amber_soft, theme.amber_fg)),
        toggle_switch(
            "dev-computer-use",
            computer_use,
            true,
            theme,
            move |_, cx| {
                hub_computer
                    .read(cx)
                    .set_computer_use_enabled(!computer_use)
            },
        ),
        theme,
    )
}

fn render_pod_window_row(pane: &DevicePane, snap: &DeviceSnapshot, theme: &Theme) -> AnyElement {
    let hub_pod = pane.hub.clone();
    let pod_visible = snap.pod_visible;
    row_frame(
        true,
        "対話ウィンドウ",
        Some(if pod_visible {
            status_pill("表示中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
        } else {
            status_pill(
                "トレイ待機中",
                Some(theme.ink_placeholder),
                theme.bg_blush,
                theme.ink_muted,
            )
        }),
        pill_button(
            "dev-pod-toggle",
            if pod_visible { "隠す" } else { "開く" },
            ButtonStyle::new(if pod_visible { Tone::Ghost } else { Tone::Soft }).maybe_icon(Some(
                if pod_visible {
                    Icon::Minimize
                } else {
                    Icon::ChatBubble
                },
            )),
            theme,
            move |_, cx| {
                hub_pod.update(cx, |hub, cx| {
                    if pod_visible {
                        hub.hide_pod(cx);
                    } else {
                        hub.open_pod(None, cx);
                    }
                });
            },
        ),
        theme,
    )
}

fn render_avatar_controls_row(
    pane: &DevicePane,
    snap: &DeviceSnapshot,
    theme: &Theme,
) -> AnyElement {
    let avatar_ready = matches!(snap.devices.avatar, Device::Ready(_));
    let hub_center = pane.hub.clone();
    let scale_chips: Vec<AnyElement> = [75.0f32, 100.0, 125.0, 150.0]
        .into_iter()
        .map(|pct| {
            let selected = (snap.avatar_scale - pct).abs() < 1.0;
            let hub_scale = pane.hub.clone();
            pill_button(
                SharedString::from(format!("dev-scale-{}", pct as u32)),
                format!("{}%", pct as u32),
                ButtonStyle::new(if selected { Tone::Primary } else { Tone::Ghost })
                    .enabled(avatar_ready),
                theme,
                move |_, cx| {
                    hub_scale.update(cx, |hub, cx| hub.set_avatar_scale(pct, cx));
                },
            )
        })
        .collect();

    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(14.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink)
                                .child("アバター表示"),
                        )
                        .child(device_pill(&snap.devices.avatar, avatar_ready, theme)),
                )
                .child(pill_button(
                    "dev-avatar-center",
                    "中央に戻す",
                    Tone::Ghost.icon(Icon::Crosshair).enabled(avatar_ready),
                    theme,
                    move |_, cx| hub_center.read(cx).center_avatar(),
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(10.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink_muted)
                        .child("表示倍率"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .children(scale_chips),
                ),
        )
        .into_any_element()
}

fn render_capabilities_row(client_caps: &[String], theme: &Theme) -> AnyElement {
    let mut cap_pills: Vec<AnyElement> = client_caps
        .iter()
        .map(|cap| kind_pill(cap.clone(), theme))
        .collect();
    if cap_pills.is_empty() {
        cap_pills.push(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("登録なし")
                .into_any_element(),
        );
    }
    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(14.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("この端末の公開機能"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.))
                .children(cap_pills),
        )
        .into_any_element()
}

fn eyes_description_note(theme: &Theme) -> AnyElement {
    div()
        .px(px(16.))
        .pb(px(12.))
        .text_xs()
        .text_color(theme.ink_muted)
        .child(if cfg!(windows) {
            "オンの間、活動情報をモデルの提供元に送ります。画像対応モデルには操作中のモニターの画像も送ります。"
        } else {
            "オンの間、ウィンドウと活動情報を、設定されたモデルの提供元に送ります。"
        })
        .into_any_element()
}

#[cfg(windows)]
fn computer_use_note(theme: &Theme) -> AnyElement {
    div()
        .px(px(16.))
        .pb(px(12.))
        .text_xs()
        .text_color(theme.ink_muted)
        .child("オンの間、手はこの端末のアプリを操作できます。作業で取得した画面は作業モデルの提供元へ送ります。")
        .into_any_element()
}
