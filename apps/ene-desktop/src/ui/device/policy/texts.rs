//! The free-text policy list, its rows and the add-text panel.

//! Structured policy rules and free-text policy management for `この端末`.

use super::DevicePane;
use crate::ui::{
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{Tone, pill_button},
};
use ene_client_core::domain::PolicyText;
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};

pub(super) fn render_policy_text_editor(
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

pub(super) fn render_policy_text_row(
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

pub(super) fn render_add_policy_text_panel(
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
        .rounded(crate::ui::theme::radius::surface())
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
                        draft_clear.update(cx, crate::input::Draft::clear);
                        pane_clear.update(cx, |this, cx| {
                            this.form.adding_policy_text_open = false;
                            cx.notify();
                        });
                    },
                )),
        )
        .into_any_element()
}
