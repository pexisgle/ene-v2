//! Per-connection-kind pickers: turn the hub's choices for one connection into
//! a dropdown spec and hand it to the shared resource picker renderer.

use super::ConnectionsPane;
use super::pickers::{ResourcePickerSpec, render_resource_picker};
use crate::input::Draft;
use crate::ui::default_embedding_dimensions;
use crate::ui::theme::Theme;
use crate::ui::widgets::DropdownOption;
use ene_client_core::{connections::split_comma, domain::ConnectionEndpoint};
use gpui::{Context, Entity};

pub(super) fn build_llm_picker(
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

pub(super) fn build_voicevox_picker(
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

pub(super) fn build_sakura_tts_picker(
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

pub(super) fn build_stt_picker(
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

pub(super) fn build_embedding_picker(
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

fn find_selected_label(options: &[DropdownOption], selected: Option<&str>) -> Option<String> {
    selected.map(|sel| {
        options
            .iter()
            .find(|o| o.value == sel)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| sel.to_owned())
    })
}
