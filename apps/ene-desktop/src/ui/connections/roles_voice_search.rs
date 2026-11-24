//! Role rows backed by a connection rather than a model slot: STT, TTS and web search.

use super::ConnectionsPane;
use super::roles_panel::{close_pane_dropdown, render_role_row_header, toggle_pane_dropdown};
use crate::ui::Hub;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button};
use ene_client_core::connections::{decode_model_choice, encode_model_choice};
use ene_client_core::domain::{ConnectionConfig, ConnectionKind};
use gpui::{AnyElement, Context, div, prelude::*, px};
use std::collections::BTreeMap;

type VoiceAssignFn = fn(&mut Hub, Option<(String, String)>, &mut Context<Hub>);

/// Connection-derived inputs shared by the STT, TTS and web-search rows.
pub(super) struct RolesContext {
    pub(super) refresh_ids: Vec<String>,
    pub(super) checking_any: bool,
    stt_models: Vec<(String, String, String)>,
    active_stt: Option<(String, String)>,
    tts_voices: Vec<(String, String, String)>,
    active_tts: Option<(String, String)>,
    pub(super) conn_names: BTreeMap<String, String>,
    pub(super) search_conns: Vec<(String, String, String)>,
    pub(super) active_search_id: Option<String>,
}

pub(super) fn collect_roles_context(hub: &Hub) -> RolesContext {
    let settings = hub.model.settings();
    let refresh_ids = settings
        .map(|s| {
            s.connections
                .iter()
                .filter(|c| {
                    c.enabled
                        && matches!(
                            c.kind,
                            ConnectionKind::Llm
                                | ConnectionKind::Embedding
                                | ConnectionKind::Stt
                                | ConnectionKind::Tts
                        )
                })
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default();
    let conn_names = settings
        .map(|s| {
            s.connections
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let search_conns = settings
        .map(|s| {
            s.connections
                .iter()
                .filter(|c| c.kind == ConnectionKind::Search)
                .map(|c| {
                    let adapter = c
                        .config
                        .as_ref()
                        .and_then(|cfg| match cfg {
                            ConnectionConfig::Search(sc) => {
                                sc.endpoint.as_ref().map(|e| e.adapter.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| "search".into());
                    (c.id.clone(), c.name.clone(), adapter)
                })
                .collect()
        })
        .unwrap_or_default();
    let active_search_id = settings.and_then(|s| {
        s.connections
            .iter()
            .find(|c| c.kind == ConnectionKind::Search && c.enabled)
            .map(|c| c.id.clone())
    });
    RolesContext {
        refresh_ids,
        checking_any: !hub.setup.checking_connections.is_empty(),
        stt_models: hub.all_stt_models(),
        active_stt: hub.active_stt_selection(),
        tts_voices: hub.all_tts_voices(),
        active_tts: hub.active_tts_selection(),
        conn_names,
        search_conns,
        active_search_id,
    }
}

pub(super) struct VoiceRoleSpec {
    title: &'static str,
    dd_key: &'static str,
    filter_key: &'static str,
    filter_placeholder: &'static str,
    pick_placeholder: &'static str,
    off_label: &'static str,
    empty_dropdown_label: &'static str,
    empty_hint: &'static str,
    add_btn_id: &'static str,
    choices: Vec<(String, String, String)>,
    active: Option<(String, String)>,
    assign: VoiceAssignFn,
}

pub(super) fn render_voice_role_row(
    pane: &mut ConnectionsPane,
    spec: VoiceRoleSpec,
    conn_names: &BTreeMap<String, String>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let current_text = resolve_voice_label(&spec, conn_names);
    let dd_open = pane.open_dropdown.as_deref() == Some(spec.dd_key);
    let filter_d = pane.drafts.draft(
        cx,
        spec.filter_key.to_owned(),
        spec.filter_placeholder,
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let options = build_voice_role_options(&spec);
    let current_value = spec
        .active
        .as_ref()
        .map(|(cid, vid)| encode_model_choice(cid, vid))
        .unwrap_or_else(|| "__off__".to_owned());
    let no_choices = spec.choices.is_empty();
    let assign_fn = spec.assign;
    let pane_toggle = cx.entity();
    let pane_pick = cx.entity();
    let hub_assign = pane.hub.clone();
    let toggle_key = spec.dd_key.to_owned();

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
                selected_value: Some(current_value.as_str()),
                selected_label: Some(current_text.as_str()),
                placeholder: spec.pick_placeholder,
                options: &options,
                open: dd_open,
                enabled: live,
                max_list_height: 200.,
                filter_draft: Some(filter_d),
                filter_text: &filter_txt,
                empty_label: spec.empty_dropdown_label,
            },
            theme,
            move |_, cx| toggle_pane_dropdown(&pane_toggle, &toggle_key, cx),
            move |picked, _, cx| {
                if picked == "__off__" {
                    hub_assign.update(cx, |hub, cx| assign_fn(hub, None, cx));
                } else if let Some((cid, vid)) = decode_model_choice(&picked) {
                    hub_assign.update(cx, |hub, cx| assign_fn(hub, Some((cid, vid)), cx));
                }
                close_pane_dropdown(&pane_pick, cx);
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

pub(super) fn render_search_role_row(
    pane: &ConnectionsPane,
    search_conns: &[(String, String, String)],
    active_search_id: Option<&String>,
    live: bool,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> AnyElement {
    let dd_key = "adv-role:search";
    let dd_open = pane.open_dropdown.as_deref() == Some(dd_key);
    let mut options = Vec::with_capacity(search_conns.len() + 1);
    options.push(DropdownOption {
        value: "__off__".to_owned(),
        label: "無効（Web 検索オフ）".to_owned(),
        detail: None,
    });
    for (cid, name, adapter) in search_conns {
        options.push(DropdownOption {
            value: cid.clone(),
            label: name.clone(),
            detail: Some(adapter.clone()),
        });
    }
    let current_value = active_search_id
        .cloned()
        .unwrap_or_else(|| "__off__".to_owned());
    let current_text = active_search_id
        .and_then(|aid| {
            search_conns
                .iter()
                .find(|(cid, _, _)| cid == aid)
                .map(|(_, name, adapter)| format!("{name} ({adapter})"))
        })
        .unwrap_or_else(|| "無効（Web 検索オフ）".to_owned());
    let pane_toggle = cx.entity();
    let pane_pick = cx.entity();
    let hub_assign = pane.hub.clone();

    div()
        .border_t_1()
        .border_color(theme.border)
        .pt(px(12.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(render_role_row_header("Web 検索", &current_text, theme))
        .child(dropdown_select(
            DropdownSpec {
                id: dd_key.to_owned(),
                selected_value: Some(current_value.as_str()),
                selected_label: Some(current_text.as_str()),
                placeholder: "検索プロバイダを選択…",
                options: &options,
                open: dd_open,
                enabled: live,
                max_list_height: 180.,
                filter_draft: None,
                filter_text: "",
                empty_label: "Web 検索接続がありません（「接続を追加」から追加してください）",
            },
            theme,
            move |_, cx| toggle_pane_dropdown(&pane_toggle, dd_key, cx),
            move |picked, _, cx| {
                let target = (picked != "__off__").then_some(picked);
                hub_assign.update(cx, |hub, cx| hub.assign_search_connection(target, cx));
                close_pane_dropdown(&pane_pick, cx);
            },
        ))
        .when(search_conns.is_empty(), |this| {
            this.child(render_missing_role_hint(
                "Web 検索接続がありません。未設定の間は外部検索を行いません。",
                "adv-role-search-add",
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
            filter_key: "adv-role:stt:filter",
            filter_placeholder: "モデル名で絞り込み…",
            pick_placeholder: "モデルを選択…",
            off_label: "無効（音声入力オフ）",
            empty_dropdown_label: "利用可能な音声認識モデルがありません（「接続を追加」から追加してください）",
            empty_hint: "音声認識（STT）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-stt-add",
            choices: ctx.stt_models.clone(),
            active: ctx.active_stt.clone(),
            assign: Hub::assign_stt_model,
        },
        VoiceRoleSpec {
            title: "音声合成（TTS）",
            dd_key: "adv-role:tts",
            filter_key: "adv-role:tts:filter",
            filter_placeholder: "話者名・接続名で絞り込み…",
            pick_placeholder: "話者を選択…",
            off_label: "無効（読み上げオフ）",
            empty_dropdown_label: "利用可能な音声合成話者がありません（「接続を追加」から追加してください）",
            empty_hint: "音声合成（TTS）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-tts-add",
            choices: ctx.tts_voices.clone(),
            active: ctx.active_tts.clone(),
            assign: Hub::assign_tts_voice,
        },
    ]
}

fn resolve_voice_label(spec: &VoiceRoleSpec, conn_names: &BTreeMap<String, String>) -> String {
    spec.active
        .as_ref()
        .map(|(cid, vid)| {
            spec.choices
                .iter()
                .find(|(c, v, _)| c == cid && v == vid)
                .map(|(_, _, label)| label.clone())
                .unwrap_or_else(|| {
                    let cname = conn_names
                        .get(cid)
                        .map(String::as_str)
                        .unwrap_or(cid.as_str());
                    format!("{cname} / {vid}")
                })
        })
        .unwrap_or_else(|| spec.off_label.to_owned())
}

fn build_voice_role_options(spec: &VoiceRoleSpec) -> Vec<DropdownOption> {
    let mut options = Vec::with_capacity(spec.choices.len() + 1);
    options.push(DropdownOption {
        value: "__off__".to_owned(),
        label: spec.off_label.to_owned(),
        detail: None,
    });
    for (cid, vid, label) in &spec.choices {
        options.push(DropdownOption {
            value: encode_model_choice(cid, vid),
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
                    this.form.adding_connection = true;
                    cx.notify();
                });
            },
        ))
}
