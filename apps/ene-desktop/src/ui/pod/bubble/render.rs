//! Presentation helpers for the pod's message stage.
//!
//! These draw from a `Model` or a `Theme` and own no state; `super`
//! composes them into the element tree.

use super::{preview_text, reply_view};
use crate::state::{Link, Model, Notice, NoticeKind};
use crate::ui::Hub;
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill, window_drag_area};
use ene_client_core::domain::RiskLevel;
use ene_client_core::domain::{ApprovalDecision, MessageRole, TurnStatus};
use gpui::{AnyElement, Entity, MouseButton, MouseDownEvent, SharedString, div, prelude::*, px};

pub(super) fn compute_badge_label(
    hub: &Hub,
    streaming_preview: &str,
    in_onboarding: bool,
) -> String {
    let companion_name = hub.model.companion().map_or("ene", |c| c.name.as_str());
    let status_word = compute_status_word(&hub.model, streaming_preview, in_onboarding);
    format!("{companion_name} ・ {status_word}")
}

pub(super) fn compute_status_word(
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
        Link::Live { .. } if model.has_active_work() => "作業中",
        Link::Live { .. } if model.audio.companion_speaking => "発話中",
        Link::Live { .. } if !model.companions.chat.sending.is_empty() => "送信中",
        Link::Live { .. } if is_streaming => "返答を考え中",
        Link::Live { .. } if model.audio.transcription.is_some() => "音声入力中",
        Link::Live { .. } => "待機中",
    }
}

pub(super) fn render_notice_strip(notice: &Notice, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
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
        .rounded(crate::ui::theme::radius::large())
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
                Tone::Soft.icon(Icon::RefreshCw),
                theme,
                move |_, cx| {
                    hub_resync.update(cx, |hub, cx| {
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

pub(super) fn render_drag_strip(expanded_chat: bool, theme: &Theme) -> gpui::Div {
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

pub(super) fn render_bubble_badge(badge_label: String, theme: &Theme) -> gpui::Div {
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

pub(super) fn render_tail_dot(theme: &Theme) -> gpui::Div {
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

pub(super) fn render_approval_header(
    what: &str,
    total_pending: usize,
    risk: RiskLevel,
    theme: &Theme,
) -> gpui::Div {
    let (label, soft, fg) = match risk {
        RiskLevel::High => ("危険な操作", theme.danger_soft, theme.danger_fg),
        RiskLevel::Moderate => ("ファイルを変更", theme.amber_soft, theme.amber_fg),
        RiskLevel::Low => ("読み取りのみ", theme.bg_blush, theme.ink_muted),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(status_pill(label, None, soft, fg))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child(format!("「{what}」の実行を許可しますか？")),
                ),
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

pub(super) fn render_approval_args(args: &str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .rounded(crate::ui::theme::radius::medium())
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

pub(super) fn render_approval_actions(
    request_id: &str,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let req_id_a = request_id.to_owned();
    let req_id_b = req_id_a.clone();
    let req_id_c = req_id_a.clone();
    let hub_a = hub.clone();
    let hub_b = hub.clone();
    let hub_c = hub.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .pt(px(2.))
        // Deny and approve are the decision. "Always allow" is not next to
        // them: a mis-click there grants permanent permission for this action,
        // so it sits below, separated, and says what it does.
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.))
                .child(pill_button(
                    SharedString::from(format!("pod-approve-{request_id}")),
                    "今回だけ許可",
                    Tone::Primary.icon(Icon::Check),
                    theme,
                    move |_, cx| {
                        hub_a.update(cx, |hub, cx| {
                            hub.resolve_approval(&req_id_a, ApprovalDecision::Approve, cx);
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
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(pill_button(
                    SharedString::from(format!("pod-always-{request_id}")),
                    "次回から常に許可",
                    Tone::Ghost,
                    theme,
                    move |_, cx| {
                        hub_b.update(cx, |hub, cx| {
                            hub.resolve_approval(&req_id_b, ApprovalDecision::AlwaysApprove, cx);
                        });
                    },
                )),
        )
}

pub(super) fn failed_turn_message(model: &Model) -> Option<String> {
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

pub(super) fn collect_timeline_rows(
    model: &Model,
    streaming_preview: &str,
    theme: &Theme,
) -> Vec<AnyElement> {
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
                .rounded(crate::ui::theme::radius::large())
                .bg(theme.danger_soft)
                .px(px(10.))
                .py(px(8.))
                .text_xs()
                .text_color(theme.danger_fg)
                .child(format!("履歴を取得できませんでした: {err}"))
                .into_any_element(),
        );
    }
    for msg in &model.companions.chat.messages {
        let is_user = msg.role == MessageRole::User;
        rows.push(render_message_bubble(
            is_user,
            SharedString::from(format!("reply-{}", msg.id)),
            msg.text.clone(),
            theme,
        ));
    }
    if !streaming_preview.is_empty() {
        rows.push(render_message_bubble(
            false,
            "streaming-reply".into(),
            preview_text(streaming_preview),
            theme,
        ));
    }
    if streaming_preview.is_empty()
        && let Some(msg) = failed_turn_message(model)
    {
        rows.push(
            div()
                .w_full()
                .rounded(crate::ui::theme::radius::large())
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

pub(super) fn render_message_bubble(
    is_user: bool,
    id: SharedString,
    body_text: String,
    theme: &Theme,
) -> AnyElement {
    let text = if is_user {
        SharedString::from(body_text).into_any_element()
    } else {
        reply_view(id, body_text).into_any_element()
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .when(is_user, gpui::Styled::items_end)
        .when(!is_user, gpui::Styled::items_start)
        .child(
            div()
                .max_w(px(300.))
                .px(px(12.))
                .py(px(8.))
                .rounded(crate::ui::theme::radius::surface())
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
                .child(div().text_sm().child(text)),
        )
        .into_any_element()
}

pub(super) fn render_mini_error_banner(reason: &str, theme: &Theme) -> gpui::Div {
    div()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.danger_soft)
        .px(px(10.))
        .py(px(5.))
        .text_xs()
        .text_color(theme.danger_fg)
        .child(format!("エラー: {reason}"))
}

pub(super) fn render_live_stt_badge(label: String, theme: &Theme) -> gpui::Div {
    div()
        .rounded(crate::ui::theme::radius::large())
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

pub(super) fn render_stop_button(
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
        Tone::Danger.icon(Icon::SquareStop).enabled(!stop_requested),
        theme,
        move |_, cx| {
            hub_stop.update(cx, crate::ui::hub::Hub::stop);
        },
    ))
}
