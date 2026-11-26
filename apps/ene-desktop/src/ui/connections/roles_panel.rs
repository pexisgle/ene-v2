use super::ConnectionsPane;
use super::roles_voice_search::{
    collect_roles_context, render_search_role_row, render_voice_role_row, voice_role_specs,
};
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button};
use crate::ui::{Hub, ModelRoleSlot};
use ene_client_core::{
    connections::{decode_model_choice, encode_model_choice},
    domain::{Companion, ModelAssignments},
};
use gpui::{AnyElement, Context, Entity, FontWeight, div, prelude::*, px};

#[derive(Clone, Copy)]
pub(super) struct ModelRoleChoices<'a> {
    pub(super) all_models: &'a [(String, String, String)],
    pub(super) embedding_models: &'a [(String, String, String)],
}

pub(super) fn render_advanced_roles_panel(
    pane: &mut ConnectionsPane,
    companion: Option<&Companion>,
    choices: &ModelRoleChoices<'_>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let Some(comp) = companion else {
        return render_no_companion_roles_hint(theme);
    };
    let assignments = comp.assignments.clone().unwrap_or_default();
    let roles_ctx = collect_roles_context(pane.hub.read(cx));

    let mut rows: Vec<AnyElement> = ModelRoleSlot::ALL
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| {
            let slot_models = if slot == ModelRoleSlot::Embedding {
                choices.embedding_models
            } else {
                choices.all_models
            };
            render_model_role_row(
                pane,
                &ModelRoleRowCtx {
                    idx,
                    slot,
                    assignments: &assignments,
                    slot_models,
                    live,
                },
                theme,
                cx,
            )
        })
        .collect();

    for spec in voice_role_specs(&roles_ctx) {
        rows.push(render_voice_role_row(
            pane,
            &spec,
            &roles_ctx.conn_names,
            live,
            theme,
            cx,
        ));
    }
    rows.push(render_search_role_row(
        pane,
        &roles_ctx.search_conns,
        roles_ctx.active_search_id.as_ref(),
        live,
        theme,
        cx,
    ));

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(render_roles_refresh_bar(
            &roles_ctx.refresh_ids,
            roles_ctx.checking_any,
            live,
            &pane.hub,
            theme,
        ))
        .children(rows)
        .into_any_element()
}

#[derive(Clone, Copy)]
struct ModelRoleRowCtx<'a> {
    idx: usize,
    slot: ModelRoleSlot,
    assignments: &'a ModelAssignments,
    slot_models: &'a [(String, String, String)],
    live: bool,
}

fn render_model_role_row(
    pane: &mut ConnectionsPane,
    row: &ModelRoleRowCtx<'_>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let ModelRoleRowCtx {
        idx,
        slot,
        assignments,
        slot_models,
        live,
    } = *row;
    let current = slot.current(assignments);
    let current_text = current.as_ref().map_or_else(
        || slot.fallback_hint().to_owned(),
        |m| {
            slot_models
                .iter()
                .find(|(cid, mid, _)| cid == &m.connection_id && mid == &m.model_id)
                .map_or_else(
                    || format!("{} / {}", m.connection_id, m.model_id),
                    |(_, _, label)| label.clone(),
                )
        },
    );

    let dd_key = format!("adv-role:{slot:?}");
    let dd_open = pane.open_dropdown.as_deref() == Some(dd_key.as_str());
    let filter_d = pane.drafts.draft(
        cx,
        format!("adv-role:{slot:?}:filter"),
        "モデル名で絞り込み…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let options = build_slot_model_options(slot, slot_models);
    let current_value = current
        .as_ref()
        .map(|m| encode_model_choice(&m.connection_id, &m.model_id))
        .or_else(|| {
            (!matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task))
                .then(|| "__auto__".to_owned())
        });
    let pane_toggle = cx.entity();
    let pane_pick = cx.entity();
    let hub_assign = pane.hub.clone();
    let toggle_key = dd_key.clone();

    div()
        .when(idx > 0, |this| {
            this.border_t_1().border_color(theme.border).pt(px(12.))
        })
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(render_role_row_header(slot.label(), &current_text, theme))
        .child(dropdown_select(
            DropdownSpec {
                id: dd_key,
                selected_value: current_value.as_deref(),
                selected_label: Some(current_text.as_str()),
                placeholder: "モデルを選択…",
                options: &options,
                open: dd_open,
                enabled: live,
                max_list_height: 200.,
                filter_draft: Some(filter_d),
                filter_text: &filter_txt,
                empty_label: "利用可能なモデルがありません（接続を追加・確認してください）",
            },
            theme,
            move |_, cx| toggle_pane_dropdown(&pane_toggle, &toggle_key, cx),
            move |picked, _, cx| {
                if picked == "__auto__" {
                    hub_assign.update(cx, |hub, cx| hub.assign_model(slot, None, cx));
                } else if let Some((cid, mid)) = decode_model_choice(&picked) {
                    hub_assign.update(cx, |hub, cx| hub.assign_model(slot, Some((cid, mid)), cx));
                }
                close_pane_dropdown(&pane_pick, cx);
            },
        ))
        .into_any_element()
}

fn render_no_companion_roles_hint(theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .p(px(16.))
        .text_xs()
        .text_color(theme.ink_muted)
        .child("コンパニオンを選択すると、役割別のモデル割当を調整できます。")
        .into_any_element()
}

fn render_roles_refresh_bar(
    refresh_ids: &[String],
    checking_any: bool,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_refresh_all = hub.clone();
    let ids = refresh_ids.to_vec();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("役割ごとに使用するモデルをドロップダウンから割り当てます。"),
        )
        .when(!ids.is_empty(), |this| {
            this.child(pill_button(
                "adv-roles-refresh-all",
                if checking_any {
                    "モデル一覧を取得中…"
                } else {
                    "全接続のモデルを再取得"
                },
                Tone::Ghost
                    .icon(Icon::Refresh)
                    .enabled(live && !checking_any),
                theme,
                move |_, cx| {
                    hub_refresh_all.update(cx, |hub, cx| {
                        for cid in &ids {
                            hub.check_connection(cid.clone(), cx);
                        }
                    });
                },
            ))
        })
}

fn build_slot_model_options(
    slot: ModelRoleSlot,
    slot_models: &[(String, String, String)],
) -> Vec<DropdownOption> {
    let mut options = Vec::new();
    if !matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task) {
        options.push(DropdownOption {
            value: "__auto__".to_owned(),
            label: slot.fallback_hint().to_owned(),
            detail: None,
        });
    }
    for (cid, mid, label) in slot_models {
        options.push(DropdownOption {
            value: encode_model_choice(cid, mid),
            label: label.clone(),
            detail: Some(mid.clone()),
        });
    }
    options
}

pub(super) fn render_role_row_header(title: &str, current_text: &str, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .truncate()
                .child(current_text.to_owned()),
        )
}

pub(super) fn toggle_pane_dropdown(pane: &Entity<ConnectionsPane>, key: &str, cx: &mut gpui::App) {
    pane.update(cx, |this, cx| {
        this.open_dropdown = if this.open_dropdown.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_owned())
        };
        cx.notify();
    });
}

pub(super) fn close_pane_dropdown(pane: &Entity<ConnectionsPane>, cx: &mut gpui::App) {
    pane.update(cx, |this, cx| {
        this.open_dropdown = None;
        cx.notify();
    });
}
