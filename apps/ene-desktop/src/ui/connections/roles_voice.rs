//! Voice role rows backed by a connection.

use super::ConnectionsPane;
use super::roles_panel::{render_role_row, role_options};
use crate::ui::Hub;
use crate::ui::hub::voice_roles;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownSpec, Tone, pill_button};
use ene_client_core::domain::ModelAssignment;
use gpui::component::StyledExt as _;
use gpui::{AnyElement, Context, div, prelude::*, px};

type VoiceAssignFn = fn(&mut Hub, Option<(&str, &str)>, &mut Context<Hub>);

struct VoiceRoleSpec {
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

pub(super) fn render_voice_roles(
    pane: &ConnectionsPane,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> [AnyElement; 2] {
    voice_role_specs(pane.hub.read(cx))
        .map(|spec| render_voice_role_row(pane, &spec, live, theme, cx))
}

fn render_voice_role_row(
    pane: &ConnectionsPane,
    spec: &VoiceRoleSpec,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let current_text = resolve_voice_label(spec, pane.hub.read(cx));
    let options = role_options(spec.choices.iter(), Some(spec.off_label));
    let current_value = spec.active.as_ref().map(|(cid, vid)| ModelAssignment {
        connection_id: cid.clone(),
        model_id: vid.clone(),
    });
    let no_choices = spec.choices.is_empty();
    let assign_fn = spec.assign;
    let hub_assign = pane.hub.clone();

    render_role_row(
        spec.title,
        true,
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
        no_choices.then(|| {
            render_missing_role_hint(spec.empty_hint, spec.add_btn_id, theme, cx).into_any_element()
        }),
        theme,
        move |picked, _, cx| {
            let selection = picked.as_ref().map(|assignment| {
                (
                    assignment.connection_id.as_str(),
                    assignment.model_id.as_str(),
                )
            });
            hub_assign.update(cx, |hub, cx| assign_fn(hub, selection, cx));
        },
    )
}

fn voice_role_specs(hub: &Hub) -> [VoiceRoleSpec; 2] {
    [
        VoiceRoleSpec {
            title: "音声認識（STT）",
            dd_key: "adv-role:stt",
            filter_placeholder: "モデル名で絞り込み…",
            off_label: "無効（音声入力オフ）",
            empty_dropdown_label: "利用可能な音声認識モデルがありません（「接続を追加」から追加してください）",
            empty_hint: "音声認識（STT）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-stt-add",
            choices: hub.model.all_stt_models(),
            active: hub.model.active_stt_selection(),
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
            choices: hub.model.all_tts_voices(),
            active: hub.model.active_tts_selection(),
            assign: voice_roles::assign_tts_voice,
        },
    ]
}

fn resolve_voice_label(spec: &VoiceRoleSpec, hub: &Hub) -> String {
    spec.active.as_ref().map_or_else(
        || spec.off_label.to_owned(),
        |(cid, vid)| {
            spec.choices
                .iter()
                .find(|(c, v, _)| c == cid && v == vid)
                .map_or_else(
                    || {
                        let cname = hub
                            .model
                            .settings()
                            .and_then(|settings| {
                                settings
                                    .connections
                                    .iter()
                                    .rev()
                                    .find(|connection| connection.id == *cid)
                            })
                            .map_or(cid.as_str(), |connection| connection.name.as_str());
                        format!("{cname} / {vid}")
                    },
                    |(_, _, label)| label.clone(),
                )
        },
    )
}

fn render_missing_role_hint(
    hint: &'static str,
    btn_id: &'static str,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let pane_add = cx.entity();
    div()
        .h_flex()
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
