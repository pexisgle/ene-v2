//! Device policy: saved rules and free-text policies.

mod rules;
mod texts;

use super::{
    DevicePane, SKIP_TOOL_CONFIRMATION_ID, is_file_confirmation_rule, row_frame,
    tool_confirmation_request,
};
use crate::ui::{theme::Theme, widgets::disclosure_row};
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

use rules::*;
use texts::*;

pub(super) fn render_policy_rules_section(
    pane: &mut DevicePane,
    rules: &[ene_client_core::domain::PolicyRule],
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> (AnyElement, AnyElement, AnyElement, Option<AnyElement>) {
    let policy_heading = div()
        .text_size(px(18.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.ink)
        .child("許可ルール")
        .into_any_element();

    let skip_tool_confirmation = rules
        .iter()
        .any(|rule| rule.id == SKIP_TOOL_CONFIRMATION_ID);
    let skip_files_only = !skip_tool_confirmation && rules.iter().any(is_file_confirmation_rule);

    let mut rule_items: Vec<AnyElement> = rules
        .iter()
        .filter(|rule| rule.id != SKIP_TOOL_CONFIRMATION_ID && !is_file_confirmation_rule(rule))
        .map(|rule| render_policy_rule_row(pane, rule, live, theme, cx))
        .collect();

    if rule_items.is_empty() {
        rule_items.push(
            div()
                .border_t_1()
                .border_color(theme.border)
                .px(px(16.))
                .py(px(14.))
                .text_xs()
                .text_color(theme.ink_muted)
                .child("保存されているルールはありません（「常に許可」を選ぶと自動的に追加されます）。")
                .into_any_element(),
        );
    }

    let confirm_row = render_confirmation_skip_row(
        pane,
        live,
        skip_tool_confirmation,
        skip_files_only,
        theme,
        cx,
    );
    let policy_card = div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .flex()
        .flex_col()
        .child(confirm_row)
        .children(rule_items)
        .into_any_element();

    let pane_add_pol = cx.entity();
    let add_open = pane.form.adding_policy_open;
    let add_toggle = disclosure_row(
        "dev-policy-add-disclosure",
        "ルールを手動追加",
        add_open,
        theme,
        move |_, cx| {
            pane_add_pol.update(cx, |this, cx| {
                this.form.adding_policy_open = !this.form.adding_policy_open;
                cx.notify();
            });
        },
    );
    let add_panel = add_open.then(|| render_add_policy_panel(pane, live, theme, cx));

    (policy_heading, policy_card, add_toggle, add_panel)
}

pub(super) fn render_policy_texts_section(
    pane: &mut DevicePane,
    texts: &[ene_client_core::domain::PolicyText],
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> (AnyElement, AnyElement, AnyElement, Option<AnyElement>) {
    let heading = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("自由文ポリシー（AI 補助判定の基準）"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("構造化ルールで未決の操作を Decision モデルが評価する際の参考基準（各 500 文字以内）です。強制的な禁止や毎回確認には上の構造化ルールを使ってください。"),
        )
        .into_any_element();

    let text_items: Vec<AnyElement> = texts
        .iter()
        .enumerate()
        .map(|(index, item)| {
            if pane.form.editing_policy_text_id.as_deref() == Some(item.id.as_str()) {
                render_policy_text_editor(pane, index, item, live, theme, cx)
            } else {
                render_policy_text_row(pane, index, item, live, theme, cx)
            }
        })
        .collect();

    let policy_text_card = div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .when(text_items.is_empty(), |card| {
            card.child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("自由文ポリシーはまだありません。"),
            )
        })
        .children(text_items)
        .into_any_element();

    let pane_add_poltxt = cx.entity();
    let add_txt_open = pane.form.adding_policy_text_open;
    let add_txt_toggle = disclosure_row(
        "dev-policy-text-add-disclosure",
        "自由文ポリシーを追加",
        add_txt_open,
        theme,
        move |_, cx| {
            pane_add_poltxt.update(cx, |this, cx| {
                this.form.adding_policy_text_open = !this.form.adding_policy_text_open;
                cx.notify();
            });
        },
    );
    let add_txt_panel = add_txt_open.then(|| render_add_policy_text_panel(pane, live, theme, cx));

    (heading, policy_text_card, add_txt_toggle, add_txt_panel)
}
