use super::ConnectionsPane;
use crate::input::Draft;
use crate::ui::default_embedding_dimensions;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    DropdownOption, DropdownSpec, Tone, dropdown_select, kind_pill, pill_button,
};
use ene_client_core::{connections::ConnectionResource, domain::ConnectionEndpoint};
use gpui::{App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px};

pub(super) struct PickerInput<'a> {
    pub(super) id: &'a str,
    pub(super) resource: ConnectionResource,
    pub(super) endpoint: Option<&'a ConnectionEndpoint>,
    pub(super) list_d: &'a Entity<Draft>,
    pub(super) dim_d: Option<&'a Entity<Draft>>,
    pub(super) live: bool,
}

pub(super) fn render_resource_picker(
    pane: &ConnectionsPane,
    input: &PickerInput<'_>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let PickerInput {
        id,
        resource,
        endpoint,
        list_d,
        dim_d,
        live,
    } = *input;
    let labels = PickerLabels::for_resource(resource);
    let hub = pane.hub.read(cx);
    let checking = hub.setup.checking_connections.contains_key(id);
    let choices = resource.choices(&hub.model, id);
    let selected = resource.selected(
        list_d.read(cx).text(),
        endpoint,
        choices.first().map(|(value, _)| value.as_str()),
    );
    let options: Vec<_> = choices
        .into_iter()
        .map(|(value, label)| DropdownOption {
            detail: match resource {
                ConnectionResource::Speaker => Some(format!("ID: {value}")),
                ConnectionResource::EmbeddingModel => Some(format!(
                    "推奨次元: {}",
                    default_embedding_dimensions(&value)
                )),
                _ => (label != value).then(|| value.clone()),
            },
            value,
            label,
        })
        .collect();
    let placeholder = selected.as_deref().unwrap_or(labels.placeholder).to_owned();
    let key = format!("{}:{id}", labels.dropdown);
    let pane_entity = cx.weak_entity();
    let refresh_hub = pane.hub.clone();
    let id = id.to_owned();
    let refresh_id = id.clone();
    let list_d = list_d.clone();
    let dim_d = dim_d.cloned();

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(picker_header(
            &labels,
            &id,
            options.len(),
            (checking, live),
            theme,
            move |_, cx| {
                refresh_hub.update(cx, |hub, cx| hub.check_connection(refresh_id.clone(), cx));
            },
        ))
        .child(dropdown_select(
            DropdownSpec {
                id: key,
                selected_value: selected,
                placeholder: placeholder.into(),
                options,
                enabled: live,
                max_list_height: labels.height,
                search_placeholder: labels.search,
                empty_label: labels.empty,
            },
            move |picked, _, cx| {
                list_d.update(cx, |draft, cx| draft.set(picked.clone(), cx));
                if let Some(dimensions) = &dim_d {
                    dimensions.update(cx, |draft, cx| {
                        draft.set(default_embedding_dimensions(&picked).to_string(), cx)
                    });
                }
                _ = pane_entity.update(cx, |_, cx| cx.notify());
            },
        ))
        .when_some(labels.hint, |this, hint| {
            this.child(div().text_xs().text_color(theme.ink_muted).child(hint))
        })
}

fn picker_header(
    labels: &PickerLabels,
    id: &str,
    count: usize,
    (checking, live): (bool, bool),
    theme: &Theme,
    refresh: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink_muted)
                        .child(labels.title),
                )
                .when(count > 0, |this| {
                    this.child(kind_pill(format!("{count}{}", labels.count), theme))
                }),
        )
        .child(pill_button(
            SharedString::from(format!("conn-fetch-{}-{id}", labels.fetch)),
            if checking {
                labels.checking
            } else {
                labels.refresh
            },
            Tone::Ghost.icon(Icon::Refresh).enabled(live && !checking),
            theme,
            refresh,
        ))
}

struct PickerLabels {
    dropdown: &'static str,
    fetch: &'static str,
    title: &'static str,
    count: &'static str,
    checking: &'static str,
    refresh: &'static str,
    placeholder: &'static str,
    search: &'static str,
    empty: &'static str,
    hint: Option<&'static str>,
    height: f32,
}

impl PickerLabels {
    fn for_resource(resource: ConnectionResource) -> Self {
        match resource {
            ConnectionResource::ChatModel => Self {
                dropdown: "conn-model",
                fetch: "models",
                title: "使用するモデル",
                count: "件の候補",
                checking: "取得中…",
                refresh: "モデル一覧を取得",
                placeholder: "モデルを選択…",
                search: "モデル名で絞り込み…",
                empty: "「モデル一覧を取得」を押して利用可能なモデルを読み込んでください",
                hint: None,
                height: 240.,
            },
            ConnectionResource::Speaker => Self {
                dropdown: "conn-vv-speaker",
                fetch: "vv",
                title: "既定の話者",
                count: "件取得済み",
                checking: "起動・取得中…",
                refresh: "話者一覧を取得",
                placeholder: "話者を選択…",
                search: "話者名・IDで絞り込み…",
                empty: "「話者一覧を取得」を押すとVOICEVOXを起動して話者一覧を読み込みます",
                hint: None,
                height: 240.,
            },
            ConnectionResource::Voice => Self {
                dropdown: "conn-sakura-tts",
                fetch: "sakura-tts",
                title: "話者モデル・スタイル",
                count: "件利用可能",
                checking: "取得中…",
                refresh: "話者一覧を取得",
                placeholder: "話者を選択…",
                search: "話者名・モデルIDで絞り込み…",
                empty: "話者候補がありません",
                hint: Some(
                    "※ さくらのクラウドのコントロールパネルで各音声モデルの利用規約に同意してください",
                ),
                height: 240.,
            },
            ConnectionResource::TranscriptionModel => Self {
                dropdown: "conn-stt-model",
                fetch: "stt",
                title: "音声認識モデル",
                count: "件取得済み",
                checking: "取得中…",
                refresh: "モデル一覧を取得",
                placeholder: "文字起こしモデルを選択…",
                search: "文字起こしモデルを検索…",
                empty: "利用可能なSTTモデルがありません",
                hint: None,
                height: 200.,
            },
            ConnectionResource::EmbeddingModel => Self {
                dropdown: "conn-emb-model",
                fetch: "emb",
                title: "埋め込みモデル",
                count: "件利用可能",
                checking: "確認・取得中…",
                refresh: "接続確認・取得",
                placeholder: "埋め込みモデルを選択…",
                search: "埋め込みモデルを検索…",
                empty: "利用可能な埋め込みモデルがありません",
                hint: None,
                height: 200.,
            },
        }
    }
}
