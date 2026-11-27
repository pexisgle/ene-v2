//! The policy-rule list, its rows and the add-rule panel.

//! Structured policy rules and free-text policy management for `この端末`.

use super::{DevicePane, row_frame, tool_confirmation_request};
use crate::ui::{
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{ButtonStyle, Tone, pill_button, status_pill},
};
use ene_client_core::domain::{PolicyEffect, PolicyRule};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_policy_rule_row(
    pane: &DevicePane,
    rule: &PolicyRule,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let (eff_text, dot, bg, fg) = match rule.effect {
        PolicyEffect::Allow if rule.is_inactive_search_allow() => (
            "再承認が必要",
            theme.amber,
            theme.amber_soft,
            theme.amber_fg,
        ),
        PolicyEffect::Allow => ("許可", theme.mint, theme.mint_soft, theme.mint_fg),
        PolicyEffect::Deny => ("拒否", theme.danger, theme.danger_soft, theme.danger_fg),
        PolicyEffect::AskLlm => (
            "AI判定",
            theme.accent,
            theme.accent_soft,
            theme.accent_soft_fg,
        ),
        PolicyEffect::Confirm => ("確認", theme.amber, theme.amber_soft, theme.amber_fg),
    };
    let armed = pane.form.delete_armed_policy.as_deref() == Some(rule.id.as_str());
    let label = rule.scope_label().unwrap_or(&rule.pattern);
    let reference = rule.scope_reference().map(|tag| {
        if armed {
            tag
        } else {
            tag.get(..12).unwrap_or(tag)
        }
    });
    let pane_arm = cx.entity();
    let pane_cancel = cx.entity();
    let hub_remove = pane.hub.clone();
    let arm_id = rule.id.clone();
    let del_id = rule.id.clone();

    let trailing = if armed {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(pill_button(
                SharedString::from(format!("pol-cancel-{}", rule.id)),
                "キャンセル",
                Tone::Ghost,
                theme,
                move |_, cx| {
                    pane_cancel.update(cx, |this, cx| {
                        this.form.delete_armed_policy = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                SharedString::from(format!("pol-exec-{}", rule.id)),
                "確定",
                Tone::Danger.icon(Icon::Trash).enabled(live),
                theme,
                move |_, cx| {
                    hub_remove.update(cx, |hub, cx| hub.remove_policy_rule(del_id.clone(), cx));
                },
            ))
            .into_any_element()
    } else {
        pill_button(
            SharedString::from(format!("pol-arm-{}", rule.id)),
            "削除",
            Tone::Danger.icon(Icon::Trash).enabled(live),
            theme,
            move |_, cx| {
                pane_arm.update(cx, |this, cx| {
                    this.form.delete_armed_policy = Some(arm_id.clone());
                    cx.notify();
                });
            },
        )
    };

    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(status_pill(eff_text, Some(dot), bg, fg))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child(rule.tool_name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .child(
                            div()
                                .when(!armed, gpui::Styled::truncate)
                                .child(label.to_owned()),
                        )
                        .when_some(reference, |el, tag| {
                            el.child(div().child(format!("照合 ID {tag}")))
                        }),
                ),
        )
        .child(trailing)
        .into_any_element()
}

pub(super) fn render_confirmation_skip_row(
    pane: &DevicePane,
    live: bool,
    skip_tool_confirmation: bool,
    skip_files_only: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let confirm_status = confirmation_status_pill(skip_tool_confirmation, skip_files_only, theme);
    let actions = render_confirmation_actions(
        pane,
        live,
        skip_tool_confirmation,
        skip_files_only,
        theme,
        cx,
    );
    row_frame(
        false,
        "ツール操作の確認",
        Some(confirm_status),
        actions.into_any_element(),
        theme,
    )
}

pub(super) fn confirmation_status_pill(
    skip_tool_confirmation: bool,
    skip_files_only: bool,
    theme: &Theme,
) -> AnyElement {
    if skip_tool_confirmation {
        status_pill(
            "すべて省略中",
            Some(theme.amber),
            theme.amber_soft,
            theme.amber_fg,
        )
    } else if skip_files_only {
        status_pill(
            "ファイル操作のみ省略中",
            None,
            theme.amber_soft,
            theme.amber_fg,
        )
    } else {
        status_pill(
            "操作ごとに確認中",
            Some(theme.mint),
            theme.mint_soft,
            theme.mint_fg,
        )
    }
}

fn render_confirmation_actions(
    pane: &DevicePane,
    live: bool,
    skip_tool_confirmation: bool,
    skip_files_only: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> gpui::Div {
    let skip_armed = pane.form.skip_confirm_armed;
    let hub_confirmation = pane.hub.clone();
    let hub_revoke_legacy = hub_confirmation.clone();
    let pane_arm_skip = cx.entity();
    let pane_cancel_skip = cx.entity();
    let pane_exec_skip = cx.entity();

    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .when(skip_files_only, |buttons| {
            buttons.child(pill_button(
                "dev-file-confirmation-restore",
                "省略を解除",
                Tone::Ghost.enabled(live),
                theme,
                move |_, cx| {
                    hub_revoke_legacy.update(cx, |hub, cx| {
                        hub.update_policy_rules(
                            "ツール操作の確認設定",
                            tool_confirmation_request(false),
                            cx,
                        );
                    });
                },
            ))
        })
        .when(skip_tool_confirmation, |buttons| {
            let hub_restore = hub_confirmation.clone();
            buttons.child(pill_button(
                "dev-tool-confirmation-restore",
                "確認を再開する",
                Tone::Soft.enabled(live),
                theme,
                move |_, cx| {
                    hub_restore.update(cx, |hub, cx| {
                        hub.update_policy_rules(
                            "ツール操作の確認設定",
                            tool_confirmation_request(false),
                            cx,
                        );
                    });
                },
            ))
        })
        .when(!skip_tool_confirmation && !skip_armed, |buttons| {
            buttons.child(pill_button(
                "dev-tool-confirmation",
                "確認をすべて省略…",
                Tone::Ghost.enabled(live),
                theme,
                move |_, cx| {
                    pane_arm_skip.update(cx, |this, cx| {
                        this.form.skip_confirm_armed = true;
                        cx.notify();
                    });
                },
            ))
        })
        .when(!skip_tool_confirmation && skip_armed, |buttons| {
            let hub_skip = hub_confirmation.clone();
            buttons
                .child(pill_button(
                    "dev-tool-confirmation-cancel",
                    "キャンセル",
                    Tone::Ghost,
                    theme,
                    move |_, cx| {
                        pane_cancel_skip.update(cx, |this, cx| {
                            this.form.skip_confirm_armed = false;
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    "dev-tool-confirmation-confirm",
                    "本当にすべて省略",
                    Tone::Danger.enabled(live),
                    theme,
                    move |_, cx| {
                        pane_exec_skip.update(cx, |this, cx| {
                            this.form.skip_confirm_armed = false;
                            cx.notify();
                        });
                        hub_skip.update(cx, |hub, cx| {
                            hub.update_policy_rules(
                                "ツール操作の確認設定",
                                tool_confirmation_request(true),
                                cx,
                            );
                        });
                    },
                ))
        })
}

pub(super) fn render_add_policy_panel(
    pane: &mut DevicePane,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let tool_d = pane.drafts.draft(
        cx,
        "policy:tool",
        "例: work.write / web_search",
        None,
        theme,
    );
    let pattern_d = pane
        .drafts
        .draft(cx, "policy:pattern", "例: * / C:/work/*", None, theme);
    let tool_val = tool_d.read(cx).text().trim().to_owned();
    let pattern_val = pattern_d.read(cx).text().trim().to_owned();
    let eff_choice = pane.form.new_policy_effect;

    let eff_chip = |eff: PolicyEffect, label: &'static str| {
        let selected = eff_choice == eff;
        let pane_chip = cx.entity();
        pill_button(
            SharedString::from(format!("dev-pol-eff-{eff:?}")),
            label,
            ButtonStyle::new(if selected { Tone::Primary } else { Tone::Ghost }),
            theme,
            move |_, cx| {
                pane_chip.update(cx, |this, cx| {
                    this.form.new_policy_effect = eff;
                    cx.notify();
                });
            },
        )
    };

    let hub_add = pane.hub.clone();
    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(labeled_field("ツール名", tool_d, theme)),
                )
                .child(div().flex_1().min_w_0().child(labeled_field(
                    "対象パターン",
                    pattern_d,
                    theme,
                ))),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_wrap()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(eff_chip(PolicyEffect::Allow, "許可"))
                        .child(eff_chip(PolicyEffect::AskLlm, "AI判定"))
                        .child(eff_chip(PolicyEffect::Confirm, "確認"))
                        .child(eff_chip(PolicyEffect::Deny, "拒否")),
                )
                .child(pill_button(
                    "dev-pol-submit",
                    "追加",
                    Tone::Primary
                        .icon(Icon::Plus)
                        .enabled(live && !tool_val.is_empty() && !pattern_val.is_empty()),
                    theme,
                    move |_, cx| {
                        hub_add.update(cx, |hub, cx| {
                            hub.add_policy_rule(
                                tool_val.clone(),
                                pattern_val.clone(),
                                eff_choice,
                                cx,
                            );
                        });
                    },
                )),
        )
        .into_any_element()
}
