//! Structured policy rules and free-text policy management for `この端末`.

use super::{
    DevicePane, DeviceSnapshot, SKIP_TOOL_CONFIRMATION_ID, is_file_confirmation_rule, row_frame,
    tool_confirmation_request,
};
use crate::ui::{
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{ButtonStyle, Tone, disclosure_row, pill_button, status_pill},
};
use ene_client_core::domain::{PolicyEffect, PolicyRule, PolicyText};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_policy_rules_section(
    pane: &mut DevicePane,
    snap: &DeviceSnapshot,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> (AnyElement, AnyElement, AnyElement, Option<AnyElement>) {
    let policy_heading = div()
        .text_size(px(18.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.ink)
        .child("許可ルール")
        .into_any_element();

    let skip_tool_confirmation = snap
        .rules
        .iter()
        .any(|rule| rule.id == SKIP_TOOL_CONFIRMATION_ID);
    let skip_files_only =
        !skip_tool_confirmation && snap.rules.iter().any(is_file_confirmation_rule);

    let mut rule_items: Vec<AnyElement> = snap
        .rules
        .iter()
        .filter(|rule| rule.id != SKIP_TOOL_CONFIRMATION_ID && !is_file_confirmation_rule(rule))
        .map(|rule| render_policy_rule_row(pane, rule, snap.live, theme, cx))
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
        snap.live,
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
        .rounded(px(14.))
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
    let add_panel = add_open.then(|| render_add_policy_panel(pane, snap.live, theme, cx));

    (policy_heading, policy_card, add_toggle, add_panel)
}

fn render_policy_rule_row(
    pane: &DevicePane,
    rule: &PolicyRule,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let (eff_text, dot, bg, fg) = match rule.effect {
        PolicyEffect::Allow => ("許可", theme.mint, theme.mint_soft, theme.mint_fg),
        PolicyEffect::Deny => ("拒否", theme.danger, theme.danger_soft, theme.danger_fg),
        PolicyEffect::AskLlm => (
            "AI判定",
            theme.accent,
            theme.accent_soft,
            theme.accent_soft_fg,
        ),
        PolicyEffect::Confirm | PolicyEffect::Unspecified => {
            ("確認", theme.amber, theme.amber_soft, theme.amber_fg)
        }
    };
    let armed = pane.form.delete_armed_policy.as_deref() == Some(rule.id.as_str());
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
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .truncate()
                        .child(rule.pattern.clone()),
                ),
        )
        .child(trailing)
        .into_any_element()
}

fn render_confirmation_skip_row(
    pane: &DevicePane,
    live: bool,
    skip_tool_confirmation: bool,
    skip_files_only: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let confirm_status = confirmation_status_pill(skip_tool_confirmation, skip_files_only, theme);
    let actions = render_confirmation_actions(
        live,
        skip_tool_confirmation,
        skip_files_only,
        pane.form.skip_confirm_armed,
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

fn confirmation_status_pill(
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
    live: bool,
    skip_tool_confirmation: bool,
    skip_files_only: bool,
    skip_armed: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> gpui::Div {
    let hub_confirmation = cx.entity().read(cx).hub.clone();
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

fn render_add_policy_panel(
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
        .rounded(px(14.))
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

pub(super) fn render_policy_texts_section(
    pane: &mut DevicePane,
    snap: &DeviceSnapshot,
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

    let text_items: Vec<AnyElement> = snap
        .policy_texts
        .iter()
        .enumerate()
        .map(|(index, item)| {
            if pane.form.editing_policy_text_id.as_deref() == Some(item.id.as_str()) {
                render_policy_text_editor(pane, index, item, snap.live, theme, cx)
            } else {
                render_policy_text_row(pane, index, item, snap.live, theme, cx)
            }
        })
        .collect();

    let policy_text_card = div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(px(14.))
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
    let add_txt_panel =
        add_txt_open.then(|| render_add_policy_text_panel(pane, snap.live, theme, cx));

    (heading, policy_text_card, add_txt_toggle, add_txt_panel)
}

fn render_policy_text_editor(
    pane: &mut DevicePane,
    index: usize,
    item: &PolicyText,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let edit_d = pane.drafts.description_draft(
        cx,
        format!("policy-text:edit:{}", item.id),
        "例: プロジェクト外への書き込みは必ず確認する",
        Some(&item.text),
        theme,
    );
    let edit_val = edit_d.read(cx).text().trim().to_owned();
    let char_count = edit_val.chars().count();
    let valid = (1..=500).contains(&char_count);
    let pane_cancel = cx.entity();
    let pane_save = cx.entity();
    let hub_save = pane.hub.clone();
    let save_id = item.id.clone();

    div()
        .when(index > 0, |this| {
            this.border_t_1().border_color(theme.border)
        })
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(edit_d)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(if char_count > 500 {
                            theme.danger_fg
                        } else {
                            theme.ink_muted
                        })
                        .child(format!("{char_count} / 500 文字")),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(pill_button(
                            SharedString::from(format!("poltxt-edit-cancel-{}", item.id)),
                            "キャンセル",
                            Tone::Ghost,
                            theme,
                            move |_, cx| {
                                pane_cancel.update(cx, |this, cx| {
                                    this.form.editing_policy_text_id = None;
                                    cx.notify();
                                });
                            },
                        ))
                        .child(pill_button(
                            SharedString::from(format!("poltxt-edit-save-{}", item.id)),
                            "保存",
                            Tone::Primary.icon(Icon::Check).enabled(live && valid),
                            theme,
                            move |_, cx| {
                                let val = edit_val.clone();
                                let id = save_id.clone();
                                hub_save.update(cx, |hub, cx| {
                                    hub.upsert_policy_text(Some(id), val, cx);
                                });
                                pane_save.update(cx, |this, cx| {
                                    this.form.editing_policy_text_id = None;
                                    cx.notify();
                                });
                            },
                        )),
                ),
        )
        .into_any_element()
}

fn render_policy_text_row(
    pane: &DevicePane,
    index: usize,
    item: &PolicyText,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let armed = pane.form.delete_armed_policy_text.as_deref() == Some(item.id.as_str());
    let pane_edit = cx.entity();
    let pane_arm = cx.entity();
    let pane_cancel = cx.entity();
    let hub_remove = pane.hub.clone();
    let edit_id = item.id.clone();
    let arm_id = item.id.clone();
    let del_id = item.id.clone();

    let trailing = if armed {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(pill_button(
                SharedString::from(format!("poltxt-cancel-{}", item.id)),
                "キャンセル",
                Tone::Ghost,
                theme,
                move |_, cx| {
                    pane_cancel.update(cx, |this, cx| {
                        this.form.delete_armed_policy_text = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                SharedString::from(format!("poltxt-exec-{}", item.id)),
                "確定",
                Tone::Danger.icon(Icon::Trash).enabled(live),
                theme,
                move |_, cx| {
                    hub_remove.update(cx, |hub, cx| hub.remove_policy_text(del_id.clone(), cx));
                },
            ))
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(pill_button(
                SharedString::from(format!("poltxt-edit-{}", item.id)),
                "編集",
                Tone::Ghost.enabled(live),
                theme,
                move |_, cx| {
                    pane_edit.update(cx, |this, cx| {
                        this.form.editing_policy_text_id = Some(edit_id.clone());
                        this.form.delete_armed_policy_text = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                SharedString::from(format!("poltxt-arm-{}", item.id)),
                "削除",
                Tone::Danger.icon(Icon::Trash).enabled(live),
                theme,
                move |_, cx| {
                    pane_arm.update(cx, |this, cx| {
                        this.form.delete_armed_policy_text = Some(arm_id.clone());
                        cx.notify();
                    });
                },
            ))
            .into_any_element()
    };

    div()
        .when(index > 0, |this| {
            this.border_t_1().border_color(theme.border)
        })
        .px(px(16.))
        .py(px(11.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.ink)
                .child(item.text.clone()),
        )
        .child(trailing)
        .into_any_element()
}

fn render_add_policy_text_panel(
    pane: &mut DevicePane,
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let new_d = pane.drafts.description_draft(
        cx,
        "policy-text:new",
        "例: 外部 URL へのデータ送信前には必ず確認を求める",
        None,
        theme,
    );
    let new_val = new_d.read(cx).text().trim().to_owned();
    let char_count = new_val.chars().count();
    let valid = (1..=500).contains(&char_count);
    let hub_add = pane.hub.clone();
    let pane_clear = cx.entity();
    let draft_clear = new_d.clone();
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
        .child(labeled_field("ポリシー文（500 文字以内）", new_d, theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(if char_count > 500 {
                            theme.danger_fg
                        } else {
                            theme.ink_muted
                        })
                        .child(format!("{char_count} / 500 文字")),
                )
                .child(pill_button(
                    "dev-poltxt-submit",
                    "追加",
                    Tone::Primary.icon(Icon::Plus).enabled(live && valid),
                    theme,
                    move |_, cx| {
                        let val = new_val.clone();
                        hub_add.update(cx, |hub, cx| {
                            hub.upsert_policy_text(None, val, cx);
                        });
                        draft_clear.update(cx, |draft, cx| draft.clear(cx));
                        pane_clear.update(cx, |this, cx| {
                            this.form.adding_policy_text_open = false;
                            cx.notify();
                        });
                    },
                )),
        )
        .into_any_element()
}
