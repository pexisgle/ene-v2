use super::ConnectionsPane;
use super::roles_voice::{collect_roles_context, render_voice_role_row, voice_role_specs};
use crate::ui::ModelRoleSlot;
use crate::ui::hub::companions as companion_actions;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, DropdownSpec, dropdown_select};
use ene_client_core::domain::{Companion, ModelAssignment, ModelAssignments};
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

#[derive(Clone, Copy)]
pub(super) struct ModelRoleChoices<'a> {
    pub(super) all_models: &'a [(String, String, String)],
    pub(super) embedding_models: &'a [(String, String, String)],
    pub(super) harnesses: &'a [(String, String, String)],
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
            let harnesses = if slot == ModelRoleSlot::Task {
                choices.harnesses
            } else {
                &[]
            };
            render_model_role_row(
                pane,
                &ModelRoleRowCtx {
                    idx,
                    slot,
                    assignments: &assignments,
                    slot_models,
                    harnesses,
                    live,
                },
                theme,
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
        .children(rows)
        .into_any_element()
}

#[derive(Clone, Copy)]
struct ModelRoleRowCtx<'a> {
    idx: usize,
    slot: ModelRoleSlot,
    assignments: &'a ModelAssignments,
    slot_models: &'a [(String, String, String)],
    harnesses: &'a [(String, String, String)],
    live: bool,
}

fn render_model_role_row(
    pane: &mut ConnectionsPane,
    row: &ModelRoleRowCtx<'_>,
    theme: &Theme,
) -> AnyElement {
    let ModelRoleRowCtx {
        idx,
        slot,
        assignments,
        slot_models,
        harnesses,
        live,
    } = *row;
    let current = slot.current(assignments);
    let current_text = current.as_ref().map_or_else(
        || slot.fallback_hint().to_owned(),
        |m| {
            slot_models
                .iter()
                .chain(harnesses)
                .find(|(cid, mid, _)| cid == &m.connection_id && mid == &m.model_id)
                .or_else(|| harnesses.iter().find(|(cid, _, _)| cid == &m.connection_id))
                .map_or_else(
                    || format!("{} / {}", m.connection_id, m.model_id),
                    |(_, _, label)| label.clone(),
                )
        },
    );

    let dd_key = format!("adv-role:{slot:?}");
    let options = build_slot_model_options(slot, slot_models.iter().chain(harnesses));
    let current_value = current.map(Some).or_else(|| {
        (!matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task)).then_some(None)
    });
    let hub_assign = pane.hub.clone();

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
                selected_value: current_value,
                placeholder: current_text.into(),
                options,
                enabled: live,
                max_list_height: 200.,
                search_placeholder: "モデル名で絞り込み…",
                empty_label: "利用可能なモデルがありません（接続を追加・確認してください）",
            },
            move |picked, _, cx| {
                hub_assign.update(cx, |hub, cx| {
                    companion_actions::assign_model(hub, slot, picked, cx)
                });
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

fn build_slot_model_options<'a>(
    slot: ModelRoleSlot,
    slot_models: impl Iterator<Item = &'a (String, String, String)>,
) -> Vec<DropdownOption<Option<ModelAssignment>>> {
    let mut options = Vec::new();
    if !matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task) {
        options.push(DropdownOption {
            value: None,
            label: slot.fallback_hint().to_owned(),
            detail: None,
        });
    }
    for (cid, mid, label) in slot_models {
        options.push(DropdownOption {
            value: Some(ModelAssignment {
                connection_id: cid.clone(),
                model_id: mid.clone(),
            }),
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
