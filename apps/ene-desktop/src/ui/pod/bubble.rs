use super::CompanionPod;
use crate::state::{Link, Model, Notice, NoticeKind};
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill, window_drag_area};
use crate::ui::{Hub, PodMode, SettingsSection};
use ene_client_core::domain::{ApprovalDecision, ApprovalRequest, MessageRole, TurnStatus};
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, SharedString, div, prelude::*, px,
};

impl CompanionPod {
    pub(super) fn render_bubble_stage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let theme = hub.theme();
        let live = hub.live();
        let expanded = hub.pod_mode == PodMode::ExpandedChat;
        let approvals = hub.model.approvals().to_vec();
        let notice = hub.model.notices.first().cloned();
        let streaming_preview: String = hub
            .model
            .companions
            .streaming
            .iter()
            .filter(|((c, _), _)| c == &hub.model.companions.chat.companion_id)
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
            self.render_approval_body(&approval, approvals.len(), &theme)
        } else if in_onboarding {
            self.render_onboarding_body(self.needs_connection(cx), &theme, cx)
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
            .when_some(notice_strip, |this, strip| this.child(strip))
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
    ) -> AnyElement {
        let what = approval
            .capability
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or(&approval.tool_name)
            .to_owned();
        let args = approval.normalized_arguments.trim().replace('\n', " ");

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(render_approval_header(&what, total_pending, theme))
            .when(!args.is_empty(), |this| {
                this.child(render_approval_args(&args, theme))
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
            .and_then(|a| a.companion.as_ref())
            .is_some();
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
            .when_some(url_pills, |this, pills| this.child(pills))
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

fn compute_badge_label(hub: &Hub, streaming_preview: &str, in_onboarding: bool) -> String {
    let companion_name = hub
        .model
        .companion()
        .map(|c| c.name.as_str())
        .unwrap_or("ene");
    let status_word = compute_status_word(&hub.model, streaming_preview, in_onboarding);
    format!("{companion_name} ・ {status_word}")
}

fn compute_status_word(
    model: &Model,
    streaming_preview: &str,
    in_onboarding: bool,
) -> &'static str {
    let is_streaming = !streaming_preview.is_empty()
        || model
            .companions
            .chat
            .turn
            .as_ref()
            .is_some_and(|t| t.status == TurnStatus::Streaming);
    match &model.connections.link {
        Link::Connecting { .. } => "接続中",
        Link::Lost { .. } | Link::Unset { .. } | Link::Unusable { .. } => "未接続",
        Link::Live { .. } if !model.approvals().is_empty() => "確認待ち",
        Link::Live { .. } if in_onboarding => "初期設定",
        Link::Live { .. } if model.companions.work_active => "作業中",
        Link::Live { .. } if model.audio.companion_speaking => "発話中",
        Link::Live { .. } if !model.companions.chat.sending.is_empty() => "送信中",
        Link::Live { .. } if is_streaming => "返答を考え中",
        Link::Live { .. } if model.audio.transcription.is_some() => "音声入力中",
        Link::Live { .. } => "待機中",
    }
}

fn render_notice_strip(notice: Notice, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
    let hub_dismiss = hub.clone();
    let hub_resync = hub.clone();
    let is_conflict = notice.kind == NoticeKind::Conflict;
    let fg = if is_conflict {
        theme.amber_fg
    } else {
        theme.danger_fg
    };
    div()
        .w_full()
        .rounded(px(12.))
        .bg(if is_conflict {
            theme.amber_soft
        } else {
            theme.danger_soft
        })
        .px(px(10.))
        .py(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(fg)
                .truncate()
                .child(format!("{}: {}", notice.context, notice.message)),
        )
        .when(is_conflict, |this| {
            this.child(pill_button(
                "pod-notice-reload",
                "再読み込み",
                Tone::Soft.icon(Icon::Refresh),
                theme,
                move |_, cx| {
                    hub_resync.update(cx, |hub, cx| {
                        hub.resnapshot();
                        hub.refresh_active_data(cx);
                        hub.dismiss_notice(0, cx);
                    });
                },
            ))
        })
        .child(
            div()
                .id("pod-notice-close")
                .flex_none()
                .p(px(4.))
                .rounded_full()
                .cursor_pointer()
                .hover(|this| this.bg(theme.bg_surface.alpha(0.6)))
                .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                    hub_dismiss.update(cx, |hub, cx| hub.dismiss_notice(0, cx));
                })
                .child(icon(Icon::Close, 11., fg)),
        )
}

fn render_drag_strip(expanded_chat: bool, theme: &Theme) -> gpui::Div {
    window_drag_area(
        div()
            .w_full()
            .h(if expanded_chat { px(18.) } else { px(10.) })
            .flex()
            .flex_row()
            .items_center()
            .justify_end(),
        false,
    )
    .when(expanded_chat, |this| {
        this.child(
            div()
                .text_size(px(11.))
                .text_color(theme.ink_placeholder)
                .child("履歴"),
        )
    })
}

fn render_bubble_badge(badge_label: String, theme: &Theme) -> gpui::Div {
    window_drag_area(
        div()
            .absolute()
            .top(px(0.))
            .left(px(16.))
            .px(px(12.))
            .py(px(4.))
            .rounded_full()
            .bg(theme.accent)
            .border_2()
            .border_color(theme.bg_surface)
            .text_color(theme.accent_fg)
            .text_xs()
            .font_weight(gpui::FontWeight::BOLD)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .child(icon(Icon::Sparkles, 12., theme.accent_fg))
            .child(badge_label),
        false,
    )
}

fn render_tail_dot(theme: &Theme) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .pr(px(12.))
        .child(window_drag_area(div().flex_1().h(px(12.)), false))
        .child(
            div()
                .size(px(12.))
                .rounded_full()
                .bg(theme.bg_surface)
                .border_2()
                .border_color(theme.border_float)
                .shadow(theme.float_shadow()),
        )
}

fn render_approval_header(what: &str, total_pending: usize, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.ink)
                .child(format!("「{what}」の実行を許可しますか？")),
        )
        .when(total_pending > 1, |this| {
            this.child(status_pill(
                format!("他{}件", total_pending - 1),
                None,
                theme.amber_soft,
                theme.amber_fg,
            ))
        })
}

fn render_approval_args(args: &str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .rounded(px(10.))
        .bg(theme.bg_blush)
        .px(px(10.))
        .py(px(6.))
        .text_xs()
        .text_color(theme.ink_muted)
        .id("approval-arguments")
        .max_h(px(96.))
        .overflow_y_scroll()
        .child(args.to_owned())
}

fn render_approval_actions(request_id: &str, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
    let req_id_a = request_id.to_owned();
    let req_id_b = req_id_a.clone();
    let req_id_c = req_id_a.clone();
    let hub_a = hub.clone();
    let hub_b = hub.clone();
    let hub_c = hub.clone();
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .pt(px(2.))
        .child(pill_button(
            SharedString::from(format!("pod-approve-{request_id}")),
            "許可",
            Tone::Primary.icon(Icon::Check),
            theme,
            move |_, cx| {
                hub_a.update(cx, |hub, cx| {
                    hub.resolve_approval(&req_id_a, ApprovalDecision::Approve, cx);
                });
            },
        ))
        .child(pill_button(
            SharedString::from(format!("pod-always-{request_id}")),
            "常に許可",
            Tone::Soft,
            theme,
            move |_, cx| {
                hub_b.update(cx, |hub, cx| {
                    hub.resolve_approval(&req_id_b, ApprovalDecision::AlwaysApprove, cx);
                });
            },
        ))
        .child(pill_button(
            SharedString::from(format!("pod-deny-{request_id}")),
            "拒否",
            Tone::Danger.icon(Icon::Close),
            theme,
            move |_, cx| {
                hub_c.update(cx, |hub, cx| {
                    hub.resolve_approval(&req_id_c, ApprovalDecision::Deny, cx);
                });
            },
        ))
}

fn failed_turn_message(model: &Model) -> Option<String> {
    model
        .companions
        .chat
        .turn
        .as_ref()
        .filter(|turn| turn.status == TurnStatus::Failed)
        .map(|turn| {
            turn.failure
                .as_ref()
                .map(|f| f.message.clone())
                .filter(|msg| !msg.trim().is_empty())
                .unwrap_or_else(|| "応答の生成に失敗しました".to_owned())
        })
}

fn collect_timeline_rows(model: &Model, streaming_preview: &str, theme: &Theme) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    if model.companions.chat.loading && model.companions.chat.messages.is_empty() {
        rows.push(
            div()
                .py(px(16.))
                .text_center()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("履歴を読み込んでいます…")
                .into_any_element(),
        );
    }
    if let Some(err) = model.companions.chat.error.as_ref() {
        rows.push(
            div()
                .rounded(px(12.))
                .bg(theme.danger_soft)
                .px(px(10.))
                .py(px(8.))
                .text_xs()
                .text_color(theme.danger_fg)
                .child(format!("履歴を取得できませんでした: {err}"))
                .into_any_element(),
        );
    }
    for (msg_idx, msg) in model.companions.chat.messages.iter().enumerate() {
        let is_user = msg.role == MessageRole::User;
        let url_pills = (!is_user)
            .then(|| render_url_pills(&msg.text, &format!("exp-{msg_idx}"), theme))
            .flatten();
        rows.push(render_message_bubble(
            is_user,
            msg.text.clone(),
            url_pills,
            theme,
        ));
    }
    if !streaming_preview.is_empty() {
        let url_pills = render_url_pills(streaming_preview, "exp-stream", theme);
        rows.push(render_message_bubble(
            false,
            preview_text(streaming_preview),
            url_pills,
            theme,
        ));
    }
    if streaming_preview.is_empty()
        && let Some(msg) = failed_turn_message(model)
    {
        rows.push(
            div()
                .w_full()
                .rounded(px(12.))
                .bg(theme.danger_soft)
                .px(px(12.))
                .py(px(8.))
                .text_xs()
                .text_color(theme.danger_fg)
                .child(format!("エラー: {msg}"))
                .into_any_element(),
        );
    }
    rows
}

fn render_message_bubble(
    is_user: bool,
    body_text: String,
    url_pills: Option<AnyElement>,
    theme: &Theme,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .when(is_user, |this| this.items_end())
        .when(!is_user, |this| this.items_start())
        .child(
            div()
                .max_w(px(300.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(14.))
                .when(is_user, |this| {
                    this.rounded_br(px(4.))
                        .bg(theme.bg_user_msg)
                        .text_color(theme.ink)
                })
                .when(!is_user, |this| {
                    this.rounded_bl(px(4.))
                        .bg(theme.bg_blush)
                        .text_color(theme.ink)
                })
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_sm().child(SharedString::from(body_text)))
                .when_some(url_pills, |this, pills| this.child(pills)),
        )
        .into_any_element()
}

fn render_mini_error_banner(reason: &str, theme: &Theme) -> gpui::Div {
    div()
        .rounded(px(10.))
        .bg(theme.danger_soft)
        .px(px(10.))
        .py(px(5.))
        .text_xs()
        .text_color(theme.danger_fg)
        .child(format!("エラー: {reason}"))
}

fn render_live_stt_badge(label: String, theme: &Theme) -> gpui::Div {
    div()
        .rounded(px(12.))
        .bg(theme.mint_soft)
        .px(px(10.))
        .py(px(5.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .child(icon(Icon::Mic, 12., theme.mint_fg))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.mint_fg)
                .truncate()
                .child(label),
        )
}

fn render_stop_button(
    id: &'static str,
    stop_requested: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_stop = hub.clone();
    div().flex().flex_row().justify_end().child(pill_button(
        id,
        if stop_requested {
            "停止中"
        } else {
            "応答を停止"
        },
        Tone::Danger.icon(Icon::Stop).enabled(!stop_requested),
        theme,
        move |_, cx| {
            hub_stop.update(cx, |hub, cx| hub.stop(cx));
        },
    ))
}

pub(super) fn active_mini_speech_text(model: &Model) -> String {
    let streaming: String = model
        .companions
        .streaming
        .iter()
        .filter(|((c, _), _)| c == &model.companions.chat.companion_id)
        .map(|(_, text)| text.as_str())
        .collect();
    if !streaming.is_empty() {
        return streaming;
    }
    if !model.companions.chat.sending.is_empty()
        || model
            .companions
            .chat
            .turn
            .as_ref()
            .is_some_and(|t| t.status == TurnStatus::Streaming)
    {
        return "返答を考えています…".to_owned();
    }
    model
        .companions
        .chat
        .messages
        .iter()
        .rev()
        .find(|m| m.role != MessageRole::User)
        .map(|m| m.text.clone())
        .unwrap_or_else(|| "こんにちは。まずはひとこと送ってみてください。".to_owned())
}

fn is_motion_leak_line(line: &str) -> bool {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(line) else {
        return false;
    };
    !items.is_empty()
        && items.iter().all(|item| {
            item.as_object().is_some_and(|obj| {
                !obj.is_empty()
                    && obj.iter().all(|(key, value)| {
                        matches!(key.as_str(), "motion" | "expression") && value.is_string()
                    })
            })
        })
}

fn strip_motion_leak(raw: &str) -> String {
    let trimmed = raw.trim();
    let first = trimmed.lines().next().unwrap_or("");
    if is_motion_leak_line(first) {
        trimmed[first.len()..].trim_start().to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn preview_text(raw: &str) -> String {
    // Hold an unfinished leading motion-control JSON line until its shape is known,
    // while preserving Markdown, URLs, code blocks and emojis verbatim.
    let (complete, tail) = raw.rsplit_once('\n').unwrap_or(("", raw));
    if complete.is_empty() && tail.trim_start().starts_with('[') {
        return String::new();
    }
    if tail.trim_start().starts_with('[') && !tail.trim_end().ends_with(']') {
        strip_motion_leak(complete)
    } else {
        strip_motion_leak(raw)
    }
}

fn short_url_label(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let trimmed = without_scheme.trim_end_matches('/');
    if trimmed.chars().count() <= 32 {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(29).collect();
        format!("{head}…")
    }
}

pub(super) fn extract_urls(raw: &str) -> Vec<(String, String)> {
    let mut urls: Vec<(String, String)> = Vec::new();
    collect_markdown_links(raw, &mut urls);
    collect_bare_urls(raw, &mut urls);
    urls
}

fn push_unique_url(urls: &mut Vec<(String, String)>, label: String, url: String) {
    if urls.len() >= 4 || urls.iter().any(|(_, existing)| existing == &url) {
        return;
    }
    let clean_label = if label.trim().is_empty() {
        short_url_label(&url)
    } else {
        label
    };
    urls.push((clean_label, url));
}

fn collect_markdown_links(raw: &str, urls: &mut Vec<(String, String)>) {
    let mut rest = raw;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        if let Some(mid) = after_open.find("](")
            && let Some(close) = after_open[mid + 2..].find(')')
        {
            let label = after_open[..mid].trim();
            let url = after_open[mid + 2..mid + 2 + close].trim();
            if url.starts_with("http://") || url.starts_with("https://") {
                push_unique_url(urls, label.to_owned(), url.to_owned());
                rest = &after_open[mid + 2 + close + 1..];
                continue;
            }
        }
        rest = after_open;
    }
}

fn collect_bare_urls(raw: &str, urls: &mut Vec<(String, String)>) {
    for token in raw.split_whitespace() {
        let candidate = token.trim_start_matches(['(', '[', '<', '"', '\'', '「', '『']);
        let Some(pos) = candidate
            .find("https://")
            .or_else(|| candidate.find("http://"))
        else {
            continue;
        };
        let slice = &candidate[pos..];
        let end = slice
            .find(|ch: char| {
                matches!(
                    ch,
                    ')' | ']' | '>' | '"' | '\'' | ',' | '。' | '、' | '」' | '』'
                ) || ch.is_whitespace()
            })
            .unwrap_or(slice.len());
        let clean = slice[..end].trim_end_matches(['.', ';', ':']);
        if clean.starts_with("http://") || clean.starts_with("https://") {
            push_unique_url(urls, short_url_label(clean), clean.to_owned());
        }
    }
}

fn render_url_pills(raw: &str, id_prefix: &str, theme: &Theme) -> Option<AnyElement> {
    let urls = extract_urls(raw);
    if urls.is_empty() {
        return None;
    }
    let chips: Vec<AnyElement> = urls
        .into_iter()
        .enumerate()
        .map(|(idx, (label, url))| {
            let hover_bg = theme.accent_soft;
            let open_url = url.clone();
            div()
                .id(SharedString::from(format!("{id_prefix}-url-{idx}")))
                .rounded_full()
                .bg(theme.accent_soft)
                .border_1()
                .border_color(theme.border)
                .px(px(8.))
                .py(px(2.))
                .text_xs()
                .text_color(theme.accent_soft_fg)
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .on_click(move |_, _, cx| {
                    cx.open_url(&open_url);
                })
                .child(label)
                .into_any_element()
        })
        .collect();
    Some(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .children(chips)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_holds_partial_motion_data_and_preserves_markdown() {
        assert_eq!(preview_text("[{\"motion\":"), "");
        assert_eq!(
            preview_text("[{\"motion\":\"nod\"}]\nこんにちは！"),
            "こんにちは！"
        );
    }

    #[test]
    fn preview_preserves_markdown_links_and_extract_urls_parses_them() {
        let raw = "### 結果\n- **公式**: [Antigravity](https://example.com/docs) を確認 👍\nhttps://example.com/raw。";
        let clean = preview_text(raw);
        assert!(clean.contains("**公式**"), "{clean}");
        assert!(
            clean.contains("[Antigravity](https://example.com/docs)"),
            "{clean}"
        );
        assert!(clean.contains("👍"), "{clean}");
        let urls = extract_urls(raw);
        assert_eq!(
            urls,
            vec![
                (
                    "Antigravity".to_owned(),
                    "https://example.com/docs".to_owned()
                ),
                (
                    "example.com/raw".to_owned(),
                    "https://example.com/raw".to_owned()
                ),
            ]
        );
    }
}
