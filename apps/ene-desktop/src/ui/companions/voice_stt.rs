//! The `声を聞き取る（STT）` half of the `音声テスト` chapter: microphone stage
//! rows, capture-mode controls and the recognised-text log.

use super::voice::{VoiceSnapshot, device_mute_pill, render_connection_missing_pill};
use crate::audio::CaptureMode;
use crate::state::Device;
use crate::ui::Hub;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill};
use gpui::{AnyElement, Entity, FontWeight, div, prelude::*, px};

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

pub(super) fn render_stt_card(snap: &VoiceSnapshot, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
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
        .rounded(crate::ui::theme::radius::surface())
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
                move |_, cx| hub_mic.update(cx, super::super::hub::Hub::toggle_mic_mute),
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
                move |_, cx| hub_ptt_toggle.update(cx, super::super::hub::Hub::ptt_toggle),
            ))
            .when(ptt_active, |row| {
                row.child(pill_button(
                    "voice-ptt-cancel",
                    "キャンセル",
                    Tone::Danger.icon(Icon::Close),
                    theme,
                    move |_, cx| hub_ptt_cancel.update(cx, super::super::hub::Hub::ptt_cancel),
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
            move |_, cx| hub_clear.update(cx, super::super::hub::Hub::clear_voice_probe),
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
