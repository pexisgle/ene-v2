//! The pod's controls menu: the popover opened by the floating orb, its rows,
//! and the dispatch that turns a row press into a hub action.

use super::CompanionPod;
use crate::state::Device;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{ButtonStyle, Tone, pill_button};
use crate::ui::{PodMode, SettingsSection};
use ene_client_core::domain::ConnectionKind;
use gpui::{AnyElement, Context, MouseDownEvent, Window, div, prelude::*, px};

#[derive(Clone, Copy)]
enum ChatControl {
    Companions,
    Microphone { ready: bool },
    CaptureMode,
    PttToggle,
    PttCancel,
    Speaker { ready: bool },
    History,
    Settings,
    Close,
}

pub(super) fn render_controls(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let hub = pod.hub.read(cx);
    let mic_ready = matches!(hub.model.connections.devices.input, Device::Ready(_))
        && hub.has_audio_connection(ConnectionKind::Stt);
    let speaker_ready = matches!(hub.model.connections.devices.output, Device::Ready(_))
        && hub.has_audio_connection(ConnectionKind::Tts);
    let mic_on = mic_ready && !hub.model.audio.mic_muted;
    let is_ptt = hub.model.audio.capture_mode == crate::audio::CaptureMode::Ptt;
    let ptt_active = hub.model.audio.ptt_active;
    let speaker_on = hub.live() && speaker_ready && !hub.model.audio.output_muted;
    let expanded = hub.windows.pod_mode == PodMode::ExpandedChat;

    div()
        .id("pod-controls")
        .absolute()
        .bottom(px(52.))
        .left(px(0.))
        .right(px(0.))
        .occlude()
        .rounded(crate::ui::theme::radius::orb())
        .border_2()
        .border_color(theme.border_float)
        .bg(theme.bg_surface)
        .shadow(theme.float_shadow())
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.controls_open = false;
            cx.notify();
        }))
        .child(render_media_controls(
            mic_ready,
            mic_on,
            speaker_ready,
            speaker_on,
            theme,
            cx,
        ))
        .child(render_capture_controls(
            is_ptt, mic_on, ptt_active, theme, cx,
        ))
        .child(render_nav_controls(expanded, theme, cx))
        .into_any_element()
}

fn use_control(
    pod: &mut CompanionPod,
    action: ChatControl,
    window: &mut Window,
    cx: &mut Context<CompanionPod>,
) {
    let keep_open = matches!(action, ChatControl::CaptureMode)
        || (matches!(action, ChatControl::PttToggle) && !pod.hub.read(cx).model.audio.ptt_active);
    if !keep_open {
        pod.controls_open = false;
    }
    cx.notify();
    match action {
        ChatControl::Companions => pod.roster_open = true,
        ChatControl::History => pod.toggle_mode(window, cx),
        ChatControl::Settings => pod.hub.update(cx, |hub, cx| {
            hub.open_settings(SettingsSection::Companions, cx)
        }),
        ChatControl::Microphone { ready } => pod.hub.update(cx, |hub, cx| {
            if ready {
                hub.toggle_mic_mute(cx)
            } else {
                hub.open_settings(SettingsSection::Device, cx)
            }
        }),
        ChatControl::CaptureMode => pod
            .hub
            .update(cx, super::super::hub::Hub::toggle_capture_mode),
        ChatControl::PttToggle => pod.hub.update(cx, super::super::hub::Hub::ptt_toggle),
        ChatControl::PttCancel => pod.hub.update(cx, super::super::hub::Hub::ptt_cancel),
        ChatControl::Speaker { ready } => pod.hub.update(cx, |hub, cx| {
            if ready {
                hub.toggle_output_mute(cx)
            } else {
                hub.open_settings(SettingsSection::Device, cx)
            }
        }),
        ChatControl::Close => {
            let hub = pod.hub.read(cx);
            if hub.is_tray_resident() || hub.windows.settings_window.is_some() {
                pod.hub.update(cx, super::super::hub::Hub::hide_pod);
            } else {
                cx.quit();
            }
        }
    }
}

struct ControlButtonSpec {
    id: &'static str,
    label: &'static str,
    icon: Icon,
    tone: Tone,
    action: ChatControl,
}

fn control_button(
    spec: &ControlButtonSpec,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let pod = cx.entity();
    let action = spec.action;
    pill_button(
        spec.id,
        spec.label,
        ButtonStyle::new(spec.tone).icon(spec.icon),
        theme,
        move |window, cx| {
            pod.update(cx, |this, cx| use_control(this, action, window, cx));
        },
    )
}

fn render_media_controls(
    mic_ready: bool,
    mic_on: bool,
    speaker_ready: bool,
    speaker_on: bool,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    let mic_label = if !mic_ready {
        "マイク設定"
    } else if mic_on {
        "マイクをオフ"
    } else {
        "マイクをオン"
    };
    let speaker_label = if !speaker_ready {
        "音声設定"
    } else if speaker_on {
        "音声をオフ"
    } else {
        "音声をオン"
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-companions",
                label: "コンパニオン",
                icon: Icon::UserBook,
                tone: Tone::Ghost,
                action: ChatControl::Companions,
            },
            theme,
            cx,
        ))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-mic",
                label: mic_label,
                icon: if mic_on { Icon::Mic } else { Icon::MicOff },
                tone: if mic_on { Tone::Mint } else { Tone::Ghost },
                action: ChatControl::Microphone { ready: mic_ready },
            },
            theme,
            cx,
        ))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-speaker",
                label: speaker_label,
                icon: if speaker_on {
                    Icon::Volume
                } else {
                    Icon::VolumeOff
                },
                tone: if speaker_on { Tone::Mint } else { Tone::Ghost },
                action: ChatControl::Speaker {
                    ready: speaker_ready,
                },
            },
            theme,
            cx,
        ))
}

fn render_capture_controls(
    is_ptt: bool,
    mic_on: bool,
    ptt_active: bool,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-capture-mode",
                // Plain words: "PTT" and "VAD" are names for how the mic
                // decides when to listen, and the label says that instead.
                label: if is_ptt {
                    "押している間だけ話す"
                } else {
                    "自動で声を検出"
                },
                icon: Icon::Mic,
                tone: if is_ptt { Tone::Primary } else { Tone::Ghost },
                action: ChatControl::CaptureMode,
            },
            theme,
            cx,
        ))
        .when(is_ptt && mic_on, |row| {
            row.child(control_button(
                &ControlButtonSpec {
                    id: "pod-control-ptt-toggle",
                    label: if ptt_active {
                        "送信（話す終了）"
                    } else {
                        "押して話す"
                    },
                    icon: Icon::Mic,
                    tone: if ptt_active {
                        Tone::Mint
                    } else {
                        Tone::Primary
                    },
                    action: ChatControl::PttToggle,
                },
                theme,
                cx,
            ))
            .when(ptt_active, |row| {
                row.child(control_button(
                    &ControlButtonSpec {
                        id: "pod-control-ptt-cancel",
                        label: "キャンセル",
                        icon: Icon::Close,
                        tone: Tone::Danger,
                        action: ChatControl::PttCancel,
                    },
                    theme,
                    cx,
                ))
            })
        })
}

fn render_nav_controls(expanded: bool, theme: &Theme, cx: &Context<CompanionPod>) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-history",
                label: if expanded {
                    "履歴を閉じる"
                } else {
                    "履歴を表示"
                },
                icon: if expanded {
                    Icon::ChatCollapse
                } else {
                    Icon::ChatBubble
                },
                tone: Tone::Ghost,
                action: ChatControl::History,
            },
            theme,
            cx,
        ))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-settings",
                label: "設定",
                icon: Icon::Settings,
                tone: Tone::Ghost,
                action: ChatControl::Settings,
            },
            theme,
            cx,
        ))
        .child(control_button(
            &ControlButtonSpec {
                id: "pod-control-close",
                label: "チャットを閉じる",
                icon: Icon::Close,
                tone: Tone::Danger,
                action: ChatControl::Close,
            },
            theme,
            cx,
        ))
}
