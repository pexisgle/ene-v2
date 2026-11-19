//! `音声テスト` chapter in `SettingsStudio`: hears a typed phrase through the
//! real TTS connection, and shows a spoken phrase travelling from the
//! microphone through the voice gate to the host and back as text.
//!
//! The point of the tab is to localise a failure, so each stage is shown on its
//! own. A level bar that moves with a closed gate, or a gate that opens with no
//! bytes sent, names the broken stage without reading logs.

use super::SettingsStudio;
use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{Tone, pill_button, status_pill};
use crate::state::Device;
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

/// A level bar wide enough to read a voice level from across a desk.
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

/// One labelled row: a name on the left, a state on the right.
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

impl SettingsStudio {
    pub(crate) fn render_voice(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (
            live,
            capture,
            recognized,
            transcription,
            devices,
            has_stt,
            has_tts,
            output_muted,
            mic_muted,
            report,
        ) = {
            let hub = self.hub.read(cx);
            (
                hub.live(),
                hub.model.capture,
                hub.model.recognized.clone(),
                hub.model.transcription.clone(),
                hub.model.devices.clone(),
                hub.has_audio_connection(w::ConnectionKind::Stt),
                hub.has_audio_connection(w::ConnectionKind::Tts),
                hub.model.output_muted,
                hub.model.mic_muted,
                hub.model.speech_report.clone(),
            )
        };

        let header = div()
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
            );

        // --- Speech to text ---------------------------------------------------
        let mic_state = match (&devices.input, mic_muted) {
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
        };
        let gate_state = if capture.open {
            status_pill("発話中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
        } else {
            status_pill(
                "待機",
                Some(theme.ink_placeholder),
                theme.bg_blush,
                theme.ink_muted,
            )
        };
        let sent_state = if capture.sent > 0 {
            status_pill(
                format!("送信中 {} バイト", capture.sent),
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
        };
        let stt_state = if !has_stt {
            status_pill(
                "音声認識の接続が必要",
                None,
                theme.amber_soft,
                theme.amber_fg,
            )
        } else if live {
            status_pill("待機中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
        } else {
            status_pill(
                "未接続",
                Some(theme.danger),
                theme.danger_soft,
                theme.danger_fg,
            )
        };

        let preview = transcription
            .as_ref()
            .map(|live_stt| live_stt.text.clone())
            .filter(|text| !text.trim().is_empty());

        let mut recognized_rows: Vec<AnyElement> = Vec::new();
        for line in recognized.iter().rev().take(8) {
            recognized_rows.push(
                div()
                    .px(px(16.))
                    .py(px(8.))
                    .text_sm()
                    .text_color(theme.ink)
                    .child(line.clone())
                    .into_any_element(),
            );
        }
        if recognized_rows.is_empty() {
            recognized_rows.push(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("まだ認識結果はありません。話しかけてから1秒ほど待つと、確定した文がここに出ます。")
                    .into_any_element(),
            );
        }

        let hub_mic = self.hub.clone();
        let stt_card = div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .flex()
            .flex_col()
            .child(stage_row(
                "マイク",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(mic_state)
                    .child(pill_button(
                        "voice-mic-toggle",
                        if mic_muted {
                            "ミュート解除"
                        } else {
                            "ミュート"
                        },
                        Some(if mic_muted { Icon::Mic } else { Icon::MicOff }),
                        Tone::Ghost,
                        matches!(devices.input, Device::Ready(_)),
                        theme,
                        move |_, cx| {
                            hub_mic.update(cx, |hub, cx| hub.toggle_mic_mute(cx));
                        },
                    ))
                    .into_any_element(),
                theme,
                false,
            ))
            .child(stage_row(
                "マイク入力レベル",
                div()
                    .flex_1()
                    .min_w_0()
                    .child(meter(capture.level, theme))
                    .into_any_element(),
                theme,
                true,
            ))
            .child(stage_row("VAD（声の検出）", gate_state, theme, true))
            .child(stage_row("ホストへの送信", sent_state, theme, true))
            .child(stage_row("音声認識の接続", stt_state, theme, true))
            .child(stage_row(
                "聞き取り中",
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.ink_muted)
                    .truncate()
                    .child(preview.unwrap_or_else(|| "—".into()))
                    .into_any_element(),
                theme,
                true,
            ))
            .child(
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
                        Some(Icon::Trash),
                        Tone::Ghost,
                        !recognized.is_empty(),
                        theme,
                        {
                            let hub_clear = self.hub.clone();
                            move |_, cx| {
                                hub_clear.update(cx, |hub, cx| hub.clear_voice_probe(cx));
                            }
                        },
                    )),
            )
            .children(recognized_rows);

        // --- Text to speech ---------------------------------------------------
        let draft = self.draft(
            cx,
            "voice:tts",
            "読み上げる文（例: こんにちは、テスト中です）",
            Some("こんにちは、テスト中です。"),
            theme,
        );
        let text = draft.read(cx).text().trim().to_owned();
        let can_speak = live && has_tts && !text.is_empty() && !output_muted;

        let output_state = match (&devices.output, output_muted) {
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
        };
        let tts_state = if has_tts {
            status_pill("設定済み", Some(theme.mint), theme.mint_soft, theme.mint_fg)
        } else {
            status_pill(
                "読み上げの接続が必要",
                None,
                theme.amber_soft,
                theme.amber_fg,
            )
        };

        let hub_speak = self.hub.clone();
        let hub_out = self.hub.clone();
        let tts_card = div()
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
                                Some(if output_muted {
                                    Icon::Volume
                                } else {
                                    Icon::VolumeOff
                                }),
                                Tone::Ghost,
                                matches!(devices.output, Device::Ready(_)),
                                theme,
                                move |_, cx| {
                                    hub_out.update(cx, |hub, cx| hub.toggle_output_mute(cx));
                                },
                            ))
                            .child(pill_button(
                                "voice-speak",
                                "読み上げる",
                                Some(Icon::Volume),
                                Tone::Primary,
                                can_speak,
                                theme,
                                move |_, cx| {
                                    hub_speak
                                        .update(cx, |hub, cx| hub.speak_probe(text.clone(), cx));
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
            });

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("声を聞き取る（STT）"),
            )
            .child(stt_card)
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("声で読み上げる（TTS）"),
            )
            .child(tts_card)
            .into_any_element()
    }
}
