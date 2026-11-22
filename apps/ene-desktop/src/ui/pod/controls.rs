use super::{CompanionPod, MAX_INPUT};
use crate::state::{Device, Unsent};
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use crate::ui::widgets::{ButtonStyle, Tone, companion_badge, floating_orb_button, pill_button};
use crate::ui::{Hub, PodMode, SettingsSection};
use ene_client_core::domain::{Companion, ConnectionKind};
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};

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

pub(super) fn render_capsule_dock(
    pod: &mut CompanionPod,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let (theme, live, has_companion, unsent) = {
        let hub = pod.hub.read(cx);
        (
            hub.theme(),
            hub.live(),
            hub.model.companion().is_some(),
            hub.model.companions.chat.unsent.clone(),
        )
    };

    let look = theme.field_look(true, true);
    pod.chat_input.update(cx, |draft, _| draft.apply_look(look));

    let draft_text = pod.chat_input.read(cx).text().to_owned();
    let too_long = draft_text.chars().count() > MAX_INPUT;
    let can_send = live && has_companion && !draft_text.trim().is_empty() && !too_long;
    let unsent_rows = render_unsent_rows(unsent, live, &pod.hub, &theme);
    let controls_open = pod.controls_open;
    let pod_entity = cx.entity();
    let trigger = floating_orb_button(
        "pod-controls-toggle",
        Icon::More,
        controls_open,
        &theme,
        move |_, cx| {
            pod_entity.update(cx, |this, cx| {
                // Outside-click dismissal runs earlier in the same event.
                this.controls_open = !controls_open;
                this.roster_open = false;
                cx.notify();
            });
        },
    );

    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(6.))
        .children(unsent_rows)
        .when(too_long, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .px(px(8.))
                    .child(format!("入力文字数が上限（{MAX_INPUT}文字）を超えています")),
            )
        })
        .child(
            div()
                .relative()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(render_input_capsule(pod, can_send, &theme, cx))
                .child(trigger)
                .when(controls_open, |this| {
                    this.child(render_controls(pod, &theme, cx))
                }),
        )
        .into_any_element()
}

fn render_input_capsule(
    pod: &CompanionPod,
    can_send: bool,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .h(px(44.))
        .rounded_full()
        .bg(theme.bg_surface)
        .border_2()
        .border_color(theme.border_float)
        .shadow(theme.float_shadow())
        .px(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl(px(4.))
                .child(pod.chat_input.clone()),
        )
        .child(
            div()
                .id("pod-send-btn")
                .flex_none()
                .w(px(30.))
                .h(px(30.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(if can_send {
                    theme.accent
                } else {
                    theme.bg_blush
                })
                .when(can_send, |this| {
                    this.cursor_pointer()
                        .hover(|this| this.bg(theme.accent_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.submit(cx);
                            }),
                        )
                })
                .child(icon(
                    Icon::Send,
                    14.,
                    if can_send {
                        theme.accent_fg
                    } else {
                        theme.ink_placeholder
                    },
                )),
        )
}

fn render_controls(pod: &CompanionPod, theme: &Theme, cx: &Context<CompanionPod>) -> AnyElement {
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
        .rounded(px(18.))
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
        ChatControl::CaptureMode => pod.hub.update(cx, |hub, cx| hub.toggle_capture_mode(cx)),
        ChatControl::PttToggle => pod.hub.update(cx, |hub, cx| hub.ptt_toggle(cx)),
        ChatControl::PttCancel => pod.hub.update(cx, |hub, cx| hub.ptt_cancel(cx)),
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
                pod.hub.update(cx, |hub, cx| hub.hide_pod(cx));
            } else {
                cx.quit();
            }
        }
    }
}

pub(super) fn render_roster_popover(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let hub = pod.hub.read(cx);
    let companions = hub
        .model
        .snapshot()
        .map(|s| s.companions.clone())
        .unwrap_or_default();
    let active_id = hub.model.companion_id().map(str::to_owned);
    let client_id = hub.options.id.clone();

    let mut rows: Vec<AnyElement> = companions
        .into_iter()
        .map(|comp| {
            let active = active_id.as_deref() == Some(comp.id.as_str());
            let resident = hub.model.is_resident(&comp.id, &client_id);
            render_roster_row(comp, active, resident, &pod.hub, theme, cx)
        })
        .collect();

    if rows.is_empty() {
        rows.push(
            div()
                .px(px(10.))
                .py(px(10.))
                .text_xs()
                .text_color(theme.ink_muted)
                .child("コンパニオンがいません")
                .into_any_element(),
        );
    }

    div()
        .id("pod-roster-popover")
        .absolute()
        .bottom(px(68.))
        .right(px(14.))
        .w(px(264.))
        .bg(theme.bg_surface)
        .border_2()
        .border_color(theme.border_float)
        .shadow(theme.float_shadow())
        .rounded(px(18.))
        .p(px(8.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .id("pod-roster-scroll")
                .max_h(px(230.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(rows),
        )
        .child(render_roster_footer(&pod.hub, theme, cx))
        .into_any_element()
}

fn render_unsent_rows(
    unsent: Vec<Unsent>,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> Vec<AnyElement> {
    unsent
        .into_iter()
        .enumerate()
        .map(|(idx, item)| {
            let key = item.request_key;
            let text = item.text;
            let reason = item.reason;
            let retry_text = text.clone();
            let dismiss_key = key.clone();
            let hub_retry = hub.clone();
            let hub_dismiss = hub.clone();
            div()
                .w_full()
                .rounded(px(14.))
                .bg(theme.danger_soft)
                .px(px(12.))
                .py(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.danger_fg)
                                .truncate()
                                .child(format!("未送信: {text}")),
                        )
                        .when(!reason.trim().is_empty(), |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(reason),
                            )
                        }),
                )
                .child(pill_button(
                    SharedString::from(format!("pod-retry-{idx}")),
                    "再送",
                    Tone::Primary.icon(Icon::Refresh).enabled(live),
                    theme,
                    move |_, cx| {
                        hub_retry.update(cx, |hub, cx| hub.retry_unsent(&key, &retry_text, cx));
                    },
                ))
                .child(pill_button(
                    SharedString::from(format!("pod-dismiss-unsent-{idx}")),
                    "×",
                    Tone::Ghost,
                    theme,
                    move |_, cx| {
                        hub_dismiss.update(cx, |hub, cx| hub.dismiss_unsent(&dismiss_key, cx));
                    },
                ))
                .into_any_element()
        })
        .collect()
}

struct ControlButtonSpec {
    id: &'static str,
    label: &'static str,
    icon: Icon,
    tone: Tone,
    action: ChatControl,
}

fn control_button(
    spec: ControlButtonSpec,
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
            ControlButtonSpec {
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
            ControlButtonSpec {
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
            ControlButtonSpec {
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
            ControlButtonSpec {
                id: "pod-control-capture-mode",
                label: if is_ptt {
                    "PTT モード"
                } else {
                    "VAD モード"
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
                ControlButtonSpec {
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
                    ControlButtonSpec {
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
            ControlButtonSpec {
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
            ControlButtonSpec {
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
            ControlButtonSpec {
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

fn render_roster_row(
    comp: Companion,
    active: bool,
    resident: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let cid = comp.id.clone();
    let persona = comp
        .persona
        .as_deref()
        .map(|s| s.trim().replace('\n', " "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "話し方未設定".to_owned());
    let pick_id = cid.clone();
    let toggle_id = cid.clone();
    let hub_res = hub.clone();

    div()
        .id(SharedString::from(format!("pod-roster-{}", comp.id)))
        .px(px(10.))
        .py(px(8.))
        .rounded(px(12.))
        .bg(if active {
            theme.bg_blush
        } else {
            theme.bg_surface
        })
        .cursor_pointer()
        .hover(|this| this.bg(theme.bg_blush))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.hub
                    .update(cx, |hub, cx| hub.select_companion(&pick_id, cx));
                this.roster_open = false;
                this.focus_input(window, cx);
                cx.notify();
            }),
        )
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(companion_badge(
            &comp.name,
            &cid,
            28.,
            (active || resident).then_some(theme.mint),
            theme,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(theme.ink)
                        .truncate()
                        .child(comp.name),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.ink_muted)
                        .truncate()
                        .child(persona),
                ),
        )
        .child(pill_button(
            SharedString::from(format!("pod-roster-res-{cid}")),
            if resident { "常駐中" } else { "常駐" },
            ButtonStyle::new(if resident { Tone::Mint } else { Tone::Ghost }),
            theme,
            move |_, cx| {
                cx.stop_propagation();
                hub_res.update(cx, |hub, cx| {
                    if resident {
                        hub.release_companion(toggle_id.clone(), cx);
                    } else {
                        hub.activate_companion(toggle_id.clone(), cx);
                    }
                });
            },
        ))
        .when(active, |this| {
            this.child(icon(Icon::Check, 14., theme.accent))
        })
        .into_any_element()
}

fn render_roster_footer(hub: &Entity<Hub>, theme: &Theme, cx: &Context<CompanionPod>) -> gpui::Div {
    let hub_manage = hub.clone();
    let pod_close = cx.entity();
    div()
        .pt(px(4.))
        .border_t_1()
        .border_color(theme.border)
        .flex()
        .flex_row()
        .justify_end()
        .child(pill_button(
            "pod-roster-manage",
            "コンパニオン管理",
            Tone::Ghost.icon(Icon::Settings),
            theme,
            move |_, cx| {
                hub_manage.update(cx, |hub, cx| {
                    hub.open_settings(SettingsSection::Companions, cx);
                });
                pod_close.update(cx, |this, cx| {
                    this.roster_open = false;
                    cx.notify();
                });
            },
        ))
}
