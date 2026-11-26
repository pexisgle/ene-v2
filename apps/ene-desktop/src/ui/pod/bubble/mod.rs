//! The companion pod's message stage.
//!
//! `render` draws the element tree; `text` extracts previews and URLs.

mod render;
mod text;

use super::CompanionPod;
use crate::state::Link;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button};
use crate::ui::{PodMode, SettingsSection};
use ene_client_core::domain::{ApprovalRequest, RiskLevel};
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};

use render::*;
use text::*;
pub(super) use text::{active_mini_speech_text, extract_urls};

impl CompanionPod {
    pub(super) fn render_bubble_stage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let theme = hub.theme();
        let live = hub.live();
        let expanded = hub.windows.pod_mode == PodMode::ExpandedChat;
        let approvals = hub.model.approvals().to_vec();
        let notice = hub.model.notices.first().cloned();
        let streaming_preview: String = hub
            .model
            .companions
            .streaming
            .iter()
            .filter(|((c, _), _)| {
                hub.model
                    .companions
                    .chat
                    .companion_id
                    .as_ref()
                    .is_some_and(|id| c == id.as_str())
            })
            .map(|(_, text)| text.as_str())
            .collect();
        let speaking = hub.model.audio.companion_speaking;
        let stop_requested = hub.model.companions.chat.stop_requested;
        let is_busy = speaking || !streaming_preview.is_empty() || stop_requested;
        let in_onboarding = self.needs_connection(cx) || self.in_model_setup(cx);
        let badge_label = compute_badge_label(hub, &streaming_preview, in_onboarding);
        let notice_strip = notice.map(|n| render_notice_strip(n, &self.hub, &theme));
        let body: AnyElement = if !live {
            self.render_offline_body(&hub.model.connections.link, &theme)
        } else if let Some(approval) = approvals.first().cloned() {
            self.render_approval_body(&approval, approvals.len(), &theme, cx)
        } else if in_onboarding {
            super::onboarding::render_onboarding_body(self, self.needs_connection(cx), &theme, cx)
        } else if expanded {
            self.render_expanded_timeline(&streaming_preview, is_busy, stop_requested, &theme, cx)
        } else {
            self.render_mini_body(&streaming_preview, is_busy, stop_requested, &theme, cx)
        };

        let expanded_chat = expanded && !in_onboarding;
        let bubble = div()
            .w_full()
            .when(expanded_chat, |this| this.flex_1().min_h_0())
            .bg(theme.bg_surface)
            .border_2()
            .border_color(theme.border_float)
            .shadow(theme.float_shadow())
            .rounded_tl(px(24.))
            .rounded_tr(px(24.))
            .rounded_bl(px(24.))
            .rounded_br(px(8.))
            .px(px(16.))
            .pt(px(12.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(render_drag_strip(expanded_chat, &theme))
            .when_some(notice_strip, gpui::ParentElement::child)
            .child(body);

        div()
            .relative()
            .w_full()
            .when(expanded, |this| this.flex_1().min_h_0())
            .pt(px(14.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(bubble)
            .child(render_tail_dot(&theme))
            .child(render_bubble_badge(badge_label, &theme))
            .into_any_element()
    }

    fn render_offline_body(&self, link: &Link, theme: &Theme) -> AnyElement {
        let (title, detail) = match link {
            Link::Connecting { endpoint, .. } => {
                ("ホストへ接続しています", format!("接続先: {endpoint}"))
            }
            Link::Lost { reason, .. } => ("ホストとの接続が切れました", reason.clone()),
            Link::Unset { reason, .. } => ("接続先が見つかりません", reason.clone()),
            Link::Unusable { reason } => ("接続設定を確認してください", reason.clone()),
            Link::Live { .. } => ("接続中", String::new()),
        };
        let hub_settings = self.hub.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child(title),
            )
            .when(!detail.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .line_clamp(2)
                        .child(detail),
                )
            })
            .child(div().flex().flex_row().pt(px(2.)).child(pill_button(
                "pod-offline-device",
                "接続を確認",
                Tone::Soft.icon(Icon::Plug),
                theme,
                move |_, cx| {
                    hub_settings.update(cx, |hub, cx| {
                        hub.open_settings(SettingsSection::Device, cx);
                    });
                },
            )))
            .into_any_element()
    }

    fn render_approval_body(
        &self,
        approval: &ApprovalRequest,
        total_pending: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let what = approval
            .capability
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or(&approval.tool_name)
            .to_owned();
        let args = approval.normalized_arguments.trim().replace('\n', " ");

        // The badge a person sees is the policy engine's own judgement of this
        // action, not a guess made here.
        let risk = RiskLevel::of(
            approval.capability.as_deref(),
            &approval.tool_name,
            &approval.normalized_arguments,
        );

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(render_approval_header(&what, total_pending, risk, theme))
            .when(!args.is_empty(), |this| {
                // Collapsed by default: the raw JSON is escaped and long, and
                // the decision should not be buried under it.
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(pill_button(
                            SharedString::from(format!(
                                "pod-args-{}-{total_pending}",
                                approval.request_id
                            )),
                            if self.approval_args_open {
                                "引数の詳細を閉じる"
                            } else {
                                "引数の詳細を見る"
                            },
                            Tone::Ghost,
                            theme,
                            {
                                let open = self.approval_args_open;
                                let pod = cx.entity();
                                move |_, cx| {
                                    pod.update(cx, |this, cx| {
                                        this.approval_args_open = !open;
                                        cx.notify();
                                    });
                                }
                            },
                        ))
                        .when(self.approval_args_open, |this| {
                            this.child(render_approval_args(&args, theme))
                        }),
                )
            })
            .child(div().text_xs().child(if args.is_empty() {
                "常に許可すると、次回からこの操作を引数にかかわらず許可します。"
            } else {
                "常に許可すると、次回からこの操作を同じ引数で許可します。"
            }))
            .child(render_approval_actions(
                &approval.request_id,
                &self.hub,
                theme,
            ))
            .into_any_element()
    }

    fn render_mini_body(
        &self,
        streaming_preview: &str,
        is_busy: bool,
        stop_requested: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hub = self.hub.read(cx);
        let speech_text = if !streaming_preview.is_empty() {
            streaming_preview.to_owned()
        } else {
            active_mini_speech_text(&hub.model)
        };
        let display_text = if streaming_preview.is_empty() {
            speech_text.clone()
        } else {
            preview_text(streaming_preview)
        };
        let long_reply = display_text
            .lines()
            .map(|line| line.chars().count().max(1).div_ceil(24))
            .sum::<usize>()
            > 6;
        let url_pills = render_url_pills(&speech_text, "mini", theme);
        let failed_turn = failed_turn_message(&hub.model);
        let can_greet = hub.model.companions.chat.messages.is_empty()
            && !hub.model.companions.chat.loading
            && hub.model.companions.chat.sending.is_empty()
            && !is_busy;
        let can_send_greet = hub
            .model
            .companion()
            .and_then(|c| c.assignments.as_ref())
            .is_some_and(|a| a.companion.is_set());
        let transcription = hub.model.audio.transcription.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("pod-mini-reply-scroll")
                    .max_h(px(144.))
                    .overflow_y_scroll()
                    .text_sm()
                    .text_color(theme.ink)
                    .child(display_text),
            )
            .when(long_reply, |row| {
                let more = self.hub.clone();
                row.child(pill_button(
                    "pod-more",
                    "全文を読む",
                    Tone::Soft,
                    theme,
                    move |_, cx| {
                        more.update(cx, |hub, cx| hub.set_pod_mode(PodMode::ExpandedChat, cx))
                    },
                ))
            })
            .when(can_greet, |body| {
                let greeting = self.hub.clone();
                body.child(pill_button(
                    "pod-greet",
                    "「こんにちは」を送る",
                    Tone::Primary.icon(Icon::Send).enabled(can_send_greet),
                    theme,
                    move |_, cx| {
                        greeting.update(cx, |hub, cx| hub.send("こんにちは".into(), cx));
                    },
                ))
            })
            .when_some(url_pills, gpui::ParentElement::child)
            .when_some(failed_turn, |this, reason| {
                this.child(render_mini_error_banner(&reason, theme))
            })
            .when_some(transcription, |this, live_stt| {
                let label = if live_stt.text.trim().is_empty() {
                    "音声を認識中…".to_owned()
                } else {
                    format!("「{}」", live_stt.text)
                };
                this.child(render_live_stt_badge(label, theme))
            })
            .when(is_busy, |this| {
                this.child(render_stop_button(
                    "pod-stop-inline",
                    stop_requested,
                    &self.hub,
                    theme,
                ))
            })
            .into_any_element()
    }

    fn render_expanded_timeline(
        &self,
        streaming_preview: &str,
        is_busy: bool,
        stop_requested: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hub = self.hub.read(cx);
        let mut rows = collect_timeline_rows(&hub.model, streaming_preview, theme);
        if rows.is_empty() && !hub.model.companions.chat.loading {
            rows.push(
                div()
                    .py(px(20.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.ink_placeholder)
                    .child("会話履歴はまだありません")
                    .into_any_element(),
            );
        }
        let transcription = hub.model.audio.transcription.clone();

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("pod-history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .children(rows),
            )
            .when_some(transcription, |this, live_stt| {
                this.child(render_live_stt_badge(
                    format!("「{}」", live_stt.text),
                    theme,
                ))
            })
            .when(is_busy, |this| {
                this.child(render_stop_button(
                    "pod-stop-expanded",
                    stop_requested,
                    &self.hub,
                    theme,
                ))
            })
            .into_any_element()
    }
}
