//! `音声テスト` chapter in `SettingsStudio`: hears a typed phrase through the
//! real TTS connection, and shows a spoken phrase travelling from the
//! microphone through the voice gate to the host and back as text.

use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{Tone, pill_button, status_pill};
use super::{Hub, SettingsSection, SettingsStudio};
use crate::audio::CaptureMode;
use crate::state::{AudioState, Device, Devices};
use ene_client_core::domain::ConnectionKind;
use gpui::{AnyElement, Context, Entity, FontWeight, div, prelude::*, px};

fn meter(level: f32, theme: &Theme) -> AnyElement {
    let filled = (level.clamp(0., 1.) * 100.).round();
    div()
        .flex_1()
        .min_w_0()
        .h(px(8.))
        .rounded_full()
        .bg(theme.bg_blush)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(gpui::relative(filled / 100.))
                .rounded_full()
                .bg(if level > 0.01 {
                    theme.mint
                } else {
                    theme.ink_placeholder
                }),
        )
        .into_any_element()
}

fn stage_row(
    label: &'static str,
    state: AnyElement,
    theme: &Theme,
    top_border: bool,
) -> AnyElement {
    div()
        .when(top_border, |this| {
            this.border_t_1().border_color(theme.border)
        })
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child(label),
        )
        .child(state)
        .into_any_element()
}

struct VoiceSnapshot {
    live: bool,
    audio: AudioState,
    devices: Devices,
    has_stt: bool,
    has_tts: bool,
}

impl SettingsStudio {
    pub(crate) fn render_voice(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snap = {
            let hub = self.hub.read(cx);
            VoiceSnapshot {
                live: hub.live(),
                audio: hub.model.audio.clone(),
                devices: hub.model.connections.devices.clone(),
                has_stt: hub.has_audio_connection(ConnectionKind::Stt),
                has_tts: hub.has_audio_connection(ConnectionKind::Tts),
            }
        };

        let tts_draft = self.draft(
            cx,
            "voice:tts",
            "読み上げる文（例: こんにちは、テスト中です）",
            Some("こんにちは、テスト中です。"),
            theme,
        );
        let tts_text = tts_draft.read(cx).text().trim().to_owned();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(render_voice_header(theme))
            .child(render_section_heading("声を聞き取る（STT）", theme))
            .child(render_stt_card(&snap, &self.hub, theme))
            .child(render_section_heading("声で読み上げる（TTS）", theme))
            .child(render_tts_card(
                &snap, tts_draft, tts_text, &self.hub, theme,
            ))
            .into_any_element()
    }
}

fn render_voice_header(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(22.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("音声テスト"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("声が入らないときは、どの段で止まっているかをここで確かめられます。"),
        )
}

fn render_section_heading(title: &'static str, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(16.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.ink)
        .child(title)
}

fn device_mute_pill(dev: &Device, muted: bool, theme: &Theme) -> AnyElement {
    match (dev, muted) {
        (Device::Ready(_), true) => status_pill(
            "ミュート中",
            Some(theme.amber),
            theme.amber_soft,
            theme.amber_fg,
        ),
        (Device::Ready(_), false) => status_pill(
            "開いています",
            Some(theme.mint),
            theme.mint_soft,
            theme.mint_fg,
        ),
        (Device::Unavailable(_), _) => status_pill(
            "利用不可",
            Some(theme.danger),
            theme.danger_soft,
            theme.danger_fg,
        ),
        _ => status_pill(
            "停止中",
            Some(theme.ink_placeholder),
            theme.bg_blush,
            theme.ink_muted,
        ),
    }
}

fn render_connection_missing_pill(
    btn_id: &'static str,
    label: &'static str,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> AnyElement {
    let hub = hub.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(status_pill(label, None, theme.amber_soft, theme.amber_fg))
        .child(pill_button(
            btn_id,
            "接続設定を開く",
            Tone::Soft.icon(Icon::Plug),
            theme,
            move |_, cx| {
                hub.update(cx, |hub, cx| {
                    hub.open_settings(SettingsSection::Connections, cx);
                });
            },
        ))
        .into_any_element()
}

fn render_stt_card(snap: &VoiceSnapshot, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
    let is_ptt = snap.audio.capture_mode == CaptureMode::Ptt;
    let mic_ready = matches!(snap.devices.input, Device::Ready(_));
    let preview = snap
        .audio
        .transcription
        .as_ref()
        .map(|live_stt| live_stt.text.clone())
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "—".into());

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(px(14.))
        .flex()
        .flex_col()
        .child(render_stt_mic_row(snap, mic_ready, hub, theme))
        .child(stage_row(
            "マイク入力レベル",
            div()
                .flex_1()
                .min_w_0()
                .child(meter(snap.audio.capture.level, theme))
                .into_any_element(),
            theme,
            true,
        ))
        .child(stage_row(
            "入力モード",
            render_capture_mode_controls(snap, mic_ready, hub, theme),
            theme,
            true,
        ))
        .child(stage_row(
            if is_ptt {
                "PTT（収録状態）"
            } else {
                "VAD（声の検出）"
            },
            stt_gate_pill(snap.audio.capture.open, theme),
            theme,
            true,
        ))
        .child(stage_row(
            "ホストへの送信",
            stt_sent_pill(snap.audio.capture.sent, theme),
            theme,
            true,
        ))
        .child(stage_row(
            "音声認識の接続",
            stt_connection_pill(snap, hub, theme),
            theme,
            true,
        ))
        .child(stage_row(
            "聞き取り中",
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.ink_muted)
                .truncate()
                .child(preview)
                .into_any_element(),
            theme,
            true,
        ))
        .child(render_recognized_header(&snap.audio.recognized, hub, theme))
        .children(render_recognized_rows(&snap.audio.recognized, theme))
}

fn render_stt_mic_row(
    snap: &VoiceSnapshot,
    mic_ready: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> AnyElement {
    let mic_muted = snap.audio.mic_muted;
    let hub_mic = hub.clone();
    stage_row(
        "マイク",
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .child(device_mute_pill(&snap.devices.input, mic_muted, theme))
            .child(pill_button(
                "voice-mic-toggle",
                if mic_muted {
                    "ミュート解除"
                } else {
                    "ミュート"
                },
                Tone::Ghost
                    .icon(if mic_muted { Icon::Mic } else { Icon::MicOff })
                    .enabled(mic_ready),
                theme,
                move |_, cx| hub_mic.update(cx, |hub, cx| hub.toggle_mic_mute(cx)),
            ))
            .into_any_element(),
        theme,
        false,
    )
}

fn stt_gate_pill(open: bool, theme: &Theme) -> AnyElement {
    if open {
        status_pill("発話中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
    } else {
        status_pill(
            "待機",
            Some(theme.ink_placeholder),
            theme.bg_blush,
            theme.ink_muted,
        )
    }
}

fn stt_sent_pill(sent: usize, theme: &Theme) -> AnyElement {
    if sent > 0 {
        status_pill(
            format!("送信中 {sent} バイト"),
            Some(theme.mint),
            theme.mint_soft,
            theme.mint_fg,
        )
    } else {
        status_pill(
            "送信なし",
            Some(theme.ink_placeholder),
            theme.bg_blush,
            theme.ink_muted,
        )
    }
}

fn stt_connection_pill(snap: &VoiceSnapshot, hub: &Entity<Hub>, theme: &Theme) -> AnyElement {
    if !snap.has_stt {
        render_connection_missing_pill("voice-stt-open-conn", "音声認識の接続が必要", hub, theme)
    } else if snap.live {
        status_pill("待機中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
    } else {
        status_pill(
            "未接続",
            Some(theme.danger),
            theme.danger_soft,
            theme.danger_fg,
        )
    }
}

fn render_capture_mode_controls(
    snap: &VoiceSnapshot,
    mic_ready: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> AnyElement {
    let is_ptt = snap.audio.capture_mode == CaptureMode::Ptt;
    let ptt_active = snap.audio.ptt_active;
    let hub_vad = hub.clone();
    let hub_ptt = hub.clone();
    let hub_ptt_toggle = hub.clone();
    let hub_ptt_cancel = hub.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap(px(6.))
        .child(pill_button(
            "voice-mode-vad",
            "VAD（自動検出）",
            if !is_ptt { Tone::Primary } else { Tone::Ghost },
            theme,
            move |_, cx| {
                hub_vad.update(cx, |hub, cx| {
                    if hub.model.audio.capture_mode != CaptureMode::Vad {
                        hub.toggle_capture_mode(cx);
                    }
                });
            },
        ))
        .child(pill_button(
            "voice-mode-ptt",
            "PTT（押して話す）",
            if is_ptt { Tone::Primary } else { Tone::Ghost },
            theme,
            move |_, cx| {
                hub_ptt.update(cx, |hub, cx| {
                    if hub.model.audio.capture_mode != CaptureMode::Ptt {
                        hub.toggle_capture_mode(cx);
                    }
                });
            },
        ))
        .when(is_ptt, |row| {
            let can_toggle = mic_ready && !snap.audio.mic_muted && snap.has_stt && snap.live;
            row.child(pill_button(
                "voice-ptt-toggle",
                if ptt_active {
                    "送信（話す終了）"
                } else {
                    "押して話す"
                },
                if ptt_active { Tone::Mint } else { Tone::Soft }
                    .icon(Icon::Mic)
                    .enabled(can_toggle),
                theme,
                move |_, cx| hub_ptt_toggle.update(cx, |hub, cx| hub.ptt_toggle(cx)),
            ))
            .when(ptt_active, |row| {
                row.child(pill_button(
                    "voice-ptt-cancel",
                    "キャンセル",
                    Tone::Danger.icon(Icon::Close),
                    theme,
                    move |_, cx| hub_ptt_cancel.update(cx, |hub, cx| hub.ptt_cancel(cx)),
                ))
            })
        })
        .into_any_element()
}

fn render_recognized_header(recognized: &[String], hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
    let hub_clear = hub.clone();
    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child("認識された文"),
        )
        .child(pill_button(
            "voice-clear",
            "消去",
            Tone::Ghost
                .icon(Icon::Trash)
                .enabled(!recognized.is_empty()),
            theme,
            move |_, cx| hub_clear.update(cx, |hub, cx| hub.clear_voice_probe(cx)),
        ))
}

fn render_recognized_rows(recognized: &[String], theme: &Theme) -> Vec<AnyElement> {
    if recognized.is_empty() {
        return vec![
            div()
                .px(px(16.))
                .py(px(12.))
                .text_xs()
                .text_color(theme.ink_muted)
                .child("まだ認識結果はありません。話しかけてから1秒ほど待つと、確定した文がここに出ます。")
                .into_any_element(),
        ];
    }
    recognized
        .iter()
        .rev()
        .take(8)
        .map(|line| {
            div()
                .px(px(16.))
                .py(px(8.))
                .text_sm()
                .text_color(theme.ink)
                .child(line.clone())
                .into_any_element()
        })
        .collect()
}

fn render_tts_card(
    snap: &VoiceSnapshot,
    draft: Entity<crate::input::Draft>,
    text: String,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let can_speak = snap.live && snap.has_tts && !text.is_empty() && !snap.audio.output_muted;
    let output_state = device_mute_pill(&snap.devices.output, snap.audio.output_muted, theme);
    let tts_state = if snap.has_tts {
        status_pill("設定済み", Some(theme.mint), theme.mint_soft, theme.mint_fg)
    } else {
        render_connection_missing_pill("voice-tts-open-conn", "読み上げの接続が必要", hub, theme)
    };
    let hub_out = hub.clone();
    let hub_speak = hub.clone();
    let output_muted = snap.audio.output_muted;
    let report = snap.audio.speech_report.clone();

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(px(14.))
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(labeled_field("読み上げる文", draft, theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(tts_state)
                        .child(output_state),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(pill_button(
                            "voice-out-toggle",
                            if output_muted {
                                "音を出す"
                            } else {
                                "消音"
                            },
                            Tone::Ghost
                                .icon(if output_muted {
                                    Icon::Volume
                                } else {
                                    Icon::VolumeOff
                                })
                                .enabled(matches!(snap.devices.output, Device::Ready(_))),
                            theme,
                            move |_, cx| hub_out.update(cx, |hub, cx| hub.toggle_output_mute(cx)),
                        ))
                        .child(pill_button(
                            "voice-speak",
                            "読み上げる",
                            Tone::Primary.icon(Icon::Volume).enabled(can_speak),
                            theme,
                            move |_, cx| {
                                hub_speak.update(cx, |hub, cx| hub.speak_probe(text.clone(), cx));
                            },
                        )),
                ),
        )
        .when_some(report, |this, report| {
            this.child(
                div()
                    .rounded(px(10.))
                    .bg(if report.failed {
                        theme.danger_soft
                    } else {
                        theme.mint_soft
                    })
                    .px(px(10.))
                    .py(px(6.))
                    .text_xs()
                    .text_color(if report.failed {
                        theme.danger_fg
                    } else {
                        theme.mint_fg
                    })
                    .child(report.text),
            )
        })
}
