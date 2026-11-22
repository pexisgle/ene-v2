use super::ConnectionsPane;
use super::row_editor::is_voicevox_tts;
use crate::input::Draft;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    DropdownOption, DropdownSpec, Tone, dropdown_select, kind_pill, pill_button,
};
use crate::ui::{Hub, default_embedding_dimensions};
use ene_client_core::{
    connections::split_comma,
    domain::{Connection, ConnectionEndpoint, ConnectionKind},
};
use gpui::{Context, Entity, FontWeight, SharedString, div, prelude::*, px};
use std::rc::Rc;

pub(super) struct PickerInput<'a> {
    pub(super) conn: &'a Connection,
    pub(super) endpoint: Option<&'a ConnectionEndpoint>,
    pub(super) list_d: &'a Entity<Draft>,
    pub(super) dim_d: Option<&'a Entity<Draft>>,
    pub(super) live: bool,
}

struct ResourcePickerSpec<'a> {
    dd_key: String,
    fetch_btn_id: String,
    title: &'static str,
    badge_label: Option<String>,
    fetch_label: &'static str,
    placeholder: &'static str,
    empty_label: &'static str,
    hint: Option<&'static str>,
    list_height: f32,
    current_selected: Option<&'a str>,
    selected_label: Option<&'a str>,
    options: &'a [DropdownOption],
    filter_d: Entity<Draft>,
    filter_txt: &'a str,
    is_checking: bool,
    live: bool,
}

pub(super) fn render_kind_dropdown_picker(
    pane: &mut ConnectionsPane,
    input: PickerInput<'_>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<gpui::Div> {
    let id = input.conn.id.clone();
    let is_checking = pane.hub.read(cx).setup.checking_connections.contains(&id);
    let kind = input.conn.kind;
    if kind == ConnectionKind::Llm {
        return Some(build_llm_picker(
            pane,
            &id,
            input.list_d,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if is_voicevox_tts(input.conn, input.endpoint) {
        return Some(build_voicevox_picker(
            pane,
            (&id, input.endpoint, input.list_d),
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Tts && input.conn.is_sakura() {
        return Some(build_sakura_tts_picker(
            pane,
            &id,
            input.list_d,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Stt {
        return Some(build_stt_picker(
            pane,
            &id,
            input.endpoint,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Embedding {
        return Some(build_embedding_picker(
            pane,
            &id,
            (input.list_d, input.dim_d),
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    None
}

fn build_llm_picker(
    pane: &mut ConnectionsPane,
    id: &str,
    list_d: &Entity<Draft>,
    (is_checking, live): (bool, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let choices = pane.hub.read(cx).available_models_for_connection(id);
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:model-filter"),
        "モデル名で絞り込み…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let current_selected = split_comma(list_d.read(cx).text()).into_iter().next();
    let options: Vec<DropdownOption> = choices
        .iter()
        .map(|(mid, label)| DropdownOption {
            value: mid.clone(),
            label: label.clone(),
            detail: (label != mid).then(|| mid.clone()),
        })
        .collect();
    let selected_label = find_selected_label(&options, current_selected.as_deref());
    let list_d_pick = list_d.clone();
    let cid = id.to_owned();
    let badge_label = (!options.is_empty()).then(|| format!("{}件の候補", options.len()));
    render_resource_picker(
        pane,
        ResourcePickerSpec {
            dd_key: format!("conn-model:{id}"),
            fetch_btn_id: format!("conn-fetch-models-{id}"),
            title: "使用するモデル",
            badge_label,
            fetch_label: if is_checking {
                "取得中…"
            } else {
                "モデル一覧を取得"
            },
            placeholder: "モデルを選択…",
            empty_label: "「モデル一覧を取得」を押して利用可能なモデルを読み込んでください",
            hint: None,
            list_height: 240.,
            current_selected: current_selected.as_deref(),
            selected_label: selected_label.as_deref(),
            options: &options,
            filter_d,
            filter_txt: &filter_txt,
            is_checking,
            live,
        },
        id,
        theme,
        cx,
        move |hub, picked, cx| {
            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
            hub.select_connection_model(cid.clone(), picked, cx);
        },
    )
}

fn build_voicevox_picker(
    pane: &mut ConnectionsPane,
    (id, endpoint, list_d): (&str, Option<&ConnectionEndpoint>, &Entity<Draft>),
    (is_checking, live): (bool, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let choices = pane.hub.read(cx).voice_choices_for_connection(id);
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:vv-filter"),
        "話者名・IDで絞り込み…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let current_selected = split_comma(list_d.read(cx).text())
        .into_iter()
        .next()
        .or_else(|| {
            endpoint
                .and_then(|ep| serde_json::from_str::<serde_json::Value>(&ep.options).ok())
                .and_then(|v| v.get("speaker").and_then(|s| s.as_u64()))
                .map(|n| n.to_string())
        });
    let options: Vec<DropdownOption> = choices
        .iter()
        .map(|(sid, label)| DropdownOption {
            value: sid.clone(),
            label: label.clone(),
            detail: Some(format!("ID: {sid}")),
        })
        .collect();
    let selected_label = current_selected.as_deref().map(|sel| {
        options
            .iter()
            .find(|o| o.value == sel)
            .map(|o| format!("{} (ID: {})", o.label, o.value))
            .unwrap_or_else(|| sel.to_owned())
    });
    let list_d_pick = list_d.clone();
    let cid = id.to_owned();
    let badge_label = (!options.is_empty()).then(|| format!("{}件取得済み", options.len()));
    render_resource_picker(
        pane,
        ResourcePickerSpec {
            dd_key: format!("conn-vv-speaker:{id}"),
            fetch_btn_id: format!("conn-fetch-vv-{id}"),
            title: "既定の話者",
            badge_label,
            fetch_label: if is_checking {
                "起動・取得中…"
            } else {
                "話者一覧を取得"
            },
            placeholder: "話者を選択…",
            empty_label: "「話者一覧を取得」を押すとVOICEVOXを起動して話者一覧を読み込みます",
            hint: None,
            list_height: 240.,
            current_selected: current_selected.as_deref(),
            selected_label: selected_label.as_deref(),
            options: &options,
            filter_d,
            filter_txt: &filter_txt,
            is_checking,
            live,
        },
        id,
        theme,
        cx,
        move |hub, picked, cx| {
            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
            hub.select_voicevox_speaker(cid.clone(), picked, cx);
        },
    )
}

fn build_sakura_tts_picker(
    pane: &mut ConnectionsPane,
    id: &str,
    list_d: &Entity<Draft>,
    (is_checking, live): (bool, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let choices = pane.hub.read(cx).voice_choices_for_connection(id);
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:sakura-tts-filter"),
        "話者名・モデルIDで絞り込み…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let current_selected = split_comma(list_d.read(cx).text())
        .into_iter()
        .next()
        .or_else(|| choices.first().map(|(vid, _)| vid.clone()));
    let options: Vec<DropdownOption> = choices
        .iter()
        .map(|(vid, label)| DropdownOption {
            value: vid.clone(),
            label: label.clone(),
            detail: (label != vid).then(|| vid.clone()),
        })
        .collect();
    let selected_label = find_selected_label(&options, current_selected.as_deref());
    let list_d_pick = list_d.clone();
    let cid = id.to_owned();
    let badge_label = (!options.is_empty()).then(|| format!("{}件利用可能", options.len()));
    render_resource_picker(
        pane,
        ResourcePickerSpec {
            dd_key: format!("conn-sakura-tts:{id}"),
            fetch_btn_id: format!("conn-fetch-sakura-tts-{id}"),
            title: "話者モデル・スタイル",
            badge_label,
            fetch_label: if is_checking {
                "取得中…"
            } else {
                "話者一覧を取得"
            },
            placeholder: "話者を選択…",
            empty_label: "話者候補がありません",
            hint: Some(
                "※ さくらのクラウドのコントロールパネルで各音声モデルの利用規約に同意してください",
            ),
            list_height: 240.,
            current_selected: current_selected.as_deref(),
            selected_label: selected_label.as_deref(),
            options: &options,
            filter_d,
            filter_txt: &filter_txt,
            is_checking,
            live,
        },
        id,
        theme,
        cx,
        move |hub, picked, cx| {
            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
            hub.select_tts_voice(cid.clone(), picked, cx);
        },
    )
}

fn build_stt_picker(
    pane: &mut ConnectionsPane,
    id: &str,
    endpoint: Option<&ConnectionEndpoint>,
    (is_checking, live): (bool, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let choices = pane.hub.read(cx).stt_model_choices_for_connection(id);
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:stt-filter"),
        "文字起こしモデルを検索…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let current_selected = endpoint
        .and_then(|ep| serde_json::from_str::<serde_json::Value>(&ep.options).ok())
        .and_then(|v| {
            v.get("model")
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| choices.first().map(|(mid, _)| mid.clone()));
    let options: Vec<DropdownOption> = choices
        .iter()
        .map(|(mid, label)| DropdownOption {
            value: mid.clone(),
            label: label.clone(),
            detail: (label != mid).then(|| mid.clone()),
        })
        .collect();
    let selected_label = current_selected.clone();
    let cid = id.to_owned();
    let badge_label = (!options.is_empty()).then(|| format!("{}件取得済み", options.len()));
    render_resource_picker(
        pane,
        ResourcePickerSpec {
            dd_key: format!("conn-stt-model:{id}"),
            fetch_btn_id: format!("conn-fetch-stt-{id}"),
            title: "音声認識モデル",
            badge_label,
            fetch_label: if is_checking {
                "取得中…"
            } else {
                "モデル一覧を取得"
            },
            placeholder: "文字起こしモデルを選択…",
            empty_label: "利用可能なSTTモデルがありません",
            hint: None,
            list_height: 200.,
            current_selected: current_selected.as_deref(),
            selected_label: selected_label.as_deref(),
            options: &options,
            filter_d,
            filter_txt: &filter_txt,
            is_checking,
            live,
        },
        id,
        theme,
        cx,
        move |hub, picked, cx| hub.select_stt_model(cid.clone(), picked, cx),
    )
}

fn build_embedding_picker(
    pane: &mut ConnectionsPane,
    id: &str,
    (list_d, dim_d): (&Entity<Draft>, Option<&Entity<Draft>>),
    (is_checking, live): (bool, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let choices = pane.hub.read(cx).embedding_model_choices_for_connection(id);
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:emb-filter"),
        "埋め込みモデルを検索…",
        None,
        theme,
    );
    let filter_txt = filter_d.read(cx).text().to_owned();
    let current_raw = list_d.read(cx).text().trim().to_owned();
    let current_selected = (!current_raw.is_empty())
        .then_some(current_raw)
        .or_else(|| choices.first().map(|(mid, _)| mid.clone()));
    let options: Vec<DropdownOption> = choices
        .iter()
        .map(|(mid, label)| DropdownOption {
            value: mid.clone(),
            label: label.clone(),
            detail: Some(format!("推奨次元: {}", default_embedding_dimensions(mid))),
        })
        .collect();
    let selected_label = current_selected.clone();
    let list_d_pick = list_d.clone();
    let dim_d_pick = dim_d.cloned();
    let cid = id.to_owned();
    let badge_label = (!options.is_empty()).then(|| format!("{}件利用可能", options.len()));
    render_resource_picker(
        pane,
        ResourcePickerSpec {
            dd_key: format!("conn-emb-model:{id}"),
            fetch_btn_id: format!("conn-fetch-emb-{id}"),
            title: "埋め込みモデル",
            badge_label,
            fetch_label: if is_checking {
                "確認・取得中…"
            } else {
                "接続確認・取得"
            },
            placeholder: "埋め込みモデルを選択…",
            empty_label: "利用可能な埋め込みモデルがありません",
            hint: None,
            list_height: 200.,
            current_selected: current_selected.as_deref(),
            selected_label: selected_label.as_deref(),
            options: &options,
            filter_d,
            filter_txt: &filter_txt,
            is_checking,
            live,
        },
        id,
        theme,
        cx,
        move |hub, picked, cx| {
            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
            if let Some(ref dim_draft) = dim_d_pick {
                let dims = default_embedding_dimensions(&picked);
                dim_draft.update(cx, |d, cx| d.set(dims.to_string(), cx));
            }
            hub.select_embedding_model(cid.clone(), picked, cx);
        },
    )
}

fn render_resource_picker<F>(
    pane: &ConnectionsPane,
    spec: ResourcePickerSpec<'_>,
    conn_id: &str,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
    on_select: F,
) -> gpui::Div
where
    F: Fn(&mut Hub, String, &mut Context<Hub>) + 'static,
{
    let dd_open = pane.open_dropdown.as_deref() == Some(spec.dd_key.as_str());
    let pane_toggle = cx.entity();
    let pane_pick = cx.entity();
    let hub_refresh = pane.hub.clone();
    let hub_select = pane.hub.clone();
    let refresh_id = conn_id.to_owned();
    let toggle_key = spec.dd_key.clone();
    let on_select = Rc::new(on_select);

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
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
                        .gap(px(8.))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink_muted)
                                .child(spec.title),
                        )
                        .when_some(spec.badge_label, |this, badge| {
                            this.child(kind_pill(badge, theme))
                        }),
                )
                .child(pill_button(
                    SharedString::from(spec.fetch_btn_id),
                    spec.fetch_label,
                    Tone::Ghost
                        .icon(Icon::Refresh)
                        .enabled(spec.live && !spec.is_checking),
                    theme,
                    move |_, cx| {
                        hub_refresh
                            .update(cx, |hub, cx| hub.check_connection(refresh_id.clone(), cx));
                    },
                )),
        )
        .child(dropdown_select(
            DropdownSpec {
                id: spec.dd_key,
                selected_value: spec.current_selected,
                selected_label: spec.selected_label,
                placeholder: spec.placeholder,
                options: spec.options,
                open: dd_open,
                enabled: spec.live,
                max_list_height: spec.list_height,
                filter_draft: Some(spec.filter_d),
                filter_text: spec.filter_txt,
                empty_label: spec.empty_label,
            },
            theme,
            move |_, cx| {
                pane_toggle.update(cx, |this, cx| {
                    this.open_dropdown =
                        if this.open_dropdown.as_deref() == Some(toggle_key.as_str()) {
                            None
                        } else {
                            Some(toggle_key.clone())
                        };
                    cx.notify();
                });
            },
            move |picked, _, cx| {
                let cb = Rc::clone(&on_select);
                hub_select.update(cx, move |hub, cx| cb(hub, picked, cx));
                pane_pick.update(cx, |this, cx| {
                    this.open_dropdown = None;
                    cx.notify();
                });
            },
        ))
        .when_some(spec.hint, |this, hint| {
            this.child(div().text_xs().text_color(theme.ink_muted).child(hint))
        })
}

fn find_selected_label(options: &[DropdownOption], selected: Option<&str>) -> Option<String> {
    selected.map(|sel| {
        options
            .iter()
            .find(|o| o.value == sel)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| sel.to_owned())
    })
}
