//! Hardware, audio, perception, pod window, avatar, and capability status card for `この端末`.

use super::{DevicePane, device_pill, row_frame};
use crate::ui::widgets::surface_card;
use crate::{
    state::{Device, Link},
    ui::{
        Hub,
        icons::Icon,
        theme::Theme,
        widgets::{ButtonStyle, Tone, kind_pill, pill_button, status_pill, toggle_switch},
    },
};
#[cfg(windows)]
use ene_client_core::domain::ClientInfo;
use ene_client_core::domain::ConnectionKind;
use gpui::component::StyledExt as _;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_device_status_card(
    pane: &DevicePane,
    hub: &Hub,
    live: bool,
    theme: &Theme,
) -> AnyElement {
    let client = hub
        .model
        .snapshot()
        .and_then(|snapshot| snapshot.clients.iter().find(|c| c.id == hub.options.id));
    let card = surface_card(theme)
        .v_flex()
        .child(render_active_client_row(pane, hub, live, theme))
        .child(render_audio_row(pane, hub, AudioControl::Microphone, theme))
        .child(render_audio_row(pane, hub, AudioControl::Speaker, theme))
        .child(render_eyes_row(pane, hub, live, theme))
        .child(eyes_description_note(theme));

    // `card` is shadowed rather than reassigned: the Windows-only rows append
    // through a new binding, so no platform has an unused `mut`.
    #[cfg(windows)]
    let card = card
        .child(render_computer_use_row(pane, hub, client, live, theme))
        .child(computer_use_note(theme));

    let caps = client.map_or(&[][..], |c| c.capabilities.as_slice());
    card.child(render_pod_window_row(pane, hub, theme))
        .child(render_avatar_controls_row(pane, hub, theme))
        .child(render_capabilities_row(caps, theme))
        .into_any_element()
}

fn render_active_client_row(pane: &DevicePane, hub: &Hub, live: bool, theme: &Theme) -> AnyElement {
    let (link_text, link_dot, link_bg, link_fg) = match &hub.model.connections.link {
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
        if hub.model.is_active_client(&hub.options.id) {
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
                    .enabled(live && hub.model.companion_id().is_some()),
                theme,
                move |_, cx| hub_activate.update(cx, super::super::hub::Hub::activate_this_client),
            )
        },
        theme,
    )
}

#[derive(Clone, Copy)]
enum AudioControl {
    Microphone,
    Speaker,
}

fn render_audio_row(
    pane: &DevicePane,
    hub: &Hub,
    control: AudioControl,
    theme: &Theme,
) -> AnyElement {
    let devices = &hub.model.connections.devices;
    let audio = &hub.model.audio;
    let (id, title, missing, kind, device, muted, toggle) = match control {
        AudioControl::Microphone => (
            "dev-mic-toggle",
            "声で話す",
            "音声認識の接続が必要",
            ConnectionKind::Stt,
            &devices.input,
            audio.mic_muted,
            Hub::toggle_mic_mute as fn(&mut Hub, &mut Context<Hub>),
        ),
        AudioControl::Speaker => (
            "dev-out-toggle",
            "返事を読み上げる",
            "読み上げの接続が必要",
            ConnectionKind::Tts,
            &devices.output,
            audio.output_muted,
            Hub::toggle_output_mute as fn(&mut Hub, &mut Context<Hub>),
        ),
    };
    let configured = hub.has_audio_connection(kind);
    let ready = configured && matches!(device, Device::Ready(_));
    let handle = pane.hub.clone();
    row_frame(
        true,
        title,
        Some(if configured {
            device_pill(device, !muted, theme)
        } else {
            status_pill(missing, None, theme.amber_soft, theme.amber_fg)
        }),
        toggle_switch(id, ready && !muted, ready, theme, move |_, cx| {
            handle.update(cx, toggle);
        }),
        theme,
    )
}

fn render_eyes_row(pane: &DevicePane, hub: &Hub, live: bool, theme: &Theme) -> AnyElement {
    use crate::state::EyesState;

    let eyes_supported = cfg!(any(windows, target_os = "linux"));
    let eyes_label = match (
        hub.model.perception.eyes,
        live && hub.model.is_active_client(&hub.options.id),
        hub.model.perception.eyes_status.as_deref(),
    ) {
        _ if !eyes_supported => "この環境では取得不可",
        (EyesState::Off, _, _) => "オフ",
        (EyesState::Unavailable, _, _) => "この環境では取得不可",
        (_, false, _) => "担当端末になるまで待機",
        (_, _, Some("unavailable")) => "目のモデルに接続できません",
        (_, _, Some("image")) if cfg!(windows) => "画面と活動を評価",
        (_, _, Some("events" | "image")) => "活動イベントを評価",
        _ => "モデル確認中",
    };
    let hub_eyes = pane.hub.clone();
    let enabled = hub.model.perception.eyes.enabled();
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
fn render_computer_use_row(
    pane: &DevicePane,
    hub: &Hub,
    client: Option<&ClientInfo>,
    live: bool,
    theme: &Theme,
) -> AnyElement {
    let hub_computer = pane.hub.clone();
    let computer_use = hub.model.perception.computer_use_enabled;
    let granted = client.is_some_and(|c| {
        c.granted_capabilities
            .iter()
            .any(|id| id == ene_client_core::domain::COMPUTER_USE)
    });
    row_frame(
        true,
        "手に画面操作を許可",
        (live && computer_use && !granted)
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

fn render_pod_window_row(pane: &DevicePane, hub: &Hub, theme: &Theme) -> AnyElement {
    let hub_pod = pane.hub.clone();
    let pod_visible = hub.is_pod_visible();
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
                    Icon::WindowMinimize
                } else {
                    Icon::MessageCircle
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

fn render_avatar_controls_row(pane: &DevicePane, hub: &Hub, theme: &Theme) -> AnyElement {
    let avatar_ready = matches!(hub.model.connections.devices.avatar, Device::Ready(_));
    let hub_center = pane.hub.clone();
    let scale_chips: Vec<AnyElement> = [75.0f32, 100.0, 125.0, 150.0]
        .into_iter()
        .map(|pct| {
            let selected = (hub.model.perception.avatar_scale - pct).abs() < 1.0;
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
        .v_flex()
        .gap(px(10.))
        .child(
            div()
                .h_flex()
                .justify_between()
                .gap(px(12.))
                .child(
                    div()
                        .h_flex()
                        .gap(px(10.))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink)
                                .child("アバター表示"),
                        )
                        .child(device_pill(
                            &hub.model.connections.devices.avatar,
                            avatar_ready,
                            theme,
                        )),
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
                .h_flex()
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
        .v_flex()
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
