//! Voice role rows backed by a connection.

use super::ConnectionsPane;
use super::roles_panel::render_role_row_header;
use crate::ui::Hub;
use crate::ui::hub::voice_roles;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button};
use ene_client_core::domain::ModelAssignment;
use gpui::{AnyElement, Context, div, prelude::*, px};
use std::collections::BTreeMap;

type VoiceAssignFn = fn(&mut Hub, Option<(&str, &str)>, &mut Context<Hub>);

/// Connection-derived inputs shared by STT and TTS.
pub(super) struct RolesContext {
    stt_models: Vec<(String, String, String)>,
    active_stt: Option<(String, String)>,
    tts_voices: Vec<(String, String, String)>,
    active_tts: Option<(String, String)>,
    pub(super) conn_names: BTreeMap<String, String>,
}

pub(super) fn collect_roles_context(hub: &Hub) -> RolesContext {
    let settings = hub.model.settings();
    let conn_names = settings.map_or_default(|s| {
        s.connections
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect()
    });
    RolesContext {
        stt_models: hub.model.all_stt_models(),
        active_stt: hub.model.active_stt_selection(),
        tts_voices: hub.all_tts_voices(),
        active_tts: hub.active_tts_selection(),
        conn_names,
    }
}

pub(super) struct VoiceRoleSpec {
    title: &'static str,
    dd_key: &'static str,
    filter_placeholder: &'static str,
    off_label: &'static str,
    empty_dropdown_label: &'static str,
    empty_hint: &'static str,
    add_btn_id: &'static str,
    choices: Vec<(String, String, String)>,
    active: Option<(String, String)>,
    assign: VoiceAssignFn,
}

pub(super) fn render_voice_role_row(
    pane: &ConnectionsPane,
    spec: &VoiceRoleSpec,
    conn_names: &BTreeMap<String, String>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let current_text = resolve_voice_label(spec, conn_names);
    let options = build_voice_role_options(spec);
    let current_value = spec.active.as_ref().map(|(cid, vid)| ModelAssignment {
        connection_id: cid.clone(),
        model_id: vid.clone(),
    });
    let no_choices = spec.choices.is_empty();
    let assign_fn = spec.assign;
    let hub_assign = pane.hub.clone();

    div()
        .border_t_1()
        .border_color(theme.border)
        .pt(px(12.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(render_role_row_header(spec.title, &current_text, theme))
        .child(dropdown_select(
            DropdownSpec {
                id: spec.dd_key.to_owned(),
                selected_value: Some(current_value),
                placeholder: current_text.into(),
                options,
                enabled: live,
                max_list_height: 200.,
                search_placeholder: spec.filter_placeholder,
                empty_label: spec.empty_dropdown_label,
            },
            move |picked, _, cx| {
                let selection = picked.as_ref().map(|assignment| {
                    (
                        assignment.connection_id.as_str(),
                        assignment.model_id.as_str(),
                    )
                });
                hub_assign.update(cx, |hub, cx| assign_fn(hub, selection, cx));
            },
        ))
        .when(no_choices, |this| {
            this.child(render_missing_role_hint(
                spec.empty_hint,
                spec.add_btn_id,
                theme,
                cx,
            ))
        })
        .into_any_element()
}

pub(super) fn voice_role_specs(ctx: &RolesContext) -> [VoiceRoleSpec; 2] {
    [
        VoiceRoleSpec {
            title: "音声認識（STT）",
            dd_key: "adv-role:stt",
            filter_placeholder: "モデル名で絞り込み…",
            off_label: "無効（音声入力オフ）",
            empty_dropdown_label: "利用可能な音声認識モデルがありません（「接続を追加」から追加してください）",
            empty_hint: "音声認識（STT）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-stt-add",
            choices: ctx.stt_models.clone(),
            active: ctx.active_stt.clone(),
            assign: voice_roles::assign_stt_model,
        },
        VoiceRoleSpec {
            title: "音声合成（TTS）",
            dd_key: "adv-role:tts",
            filter_placeholder: "話者名・接続名で絞り込み…",
            off_label: "無効（読み上げオフ）",
            empty_dropdown_label: "利用可能な音声合成話者がありません（「接続を追加」から追加してください）",
            empty_hint: "音声合成（TTS）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-tts-add",
            choices: ctx.tts_voices.clone(),
            active: ctx.active_tts.clone(),
            assign: voice_roles::assign_tts_voice,
        },
    ]
}

fn resolve_voice_label(spec: &VoiceRoleSpec, conn_names: &BTreeMap<String, String>) -> String {
    spec.active.as_ref().map_or_else(
        || spec.off_label.to_owned(),
        |(cid, vid)| {
            spec.choices
                .iter()
                .find(|(c, v, _)| c == cid && v == vid)
                .map_or_else(
                    || {
                        let cname = conn_names.get(cid).map_or(cid.as_str(), String::as_str);
                        format!("{cname} / {vid}")
                    },
                    |(_, _, label)| label.clone(),
                )
        },
    )
}

fn build_voice_role_options(spec: &VoiceRoleSpec) -> Vec<DropdownOption<Option<ModelAssignment>>> {
    let mut options = Vec::with_capacity(spec.choices.len() + 1);
    options.push(DropdownOption {
        value: None,
        label: spec.off_label.to_owned(),
        detail: None,
    });
    for (cid, vid, label) in &spec.choices {
        options.push(DropdownOption {
            value: Some(ModelAssignment {
                connection_id: cid.clone(),
                model_id: vid.clone(),
            }),
            label: label.clone(),
            detail: Some(vid.clone()),
        });
    }
    options
}

fn render_missing_role_hint(
    hint: &'static str,
    btn_id: &'static str,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let pane_add = cx.entity();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(div().text_xs().text_color(theme.ink_muted).child(hint))
        .child(pill_button(
            btn_id,
            "接続を追加を開く",
            Tone::Ghost.icon(Icon::Plus),
            theme,
            move |_, cx| {
                pane_add.update(cx, |this, cx| {
                    this.form.add = super::AddFlow::Presets;
                    cx.notify();
                });
            },
        ))
}
