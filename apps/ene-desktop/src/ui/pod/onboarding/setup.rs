//! Setup choices: picking the model and voice before the first turn.

use super::CompanionPod;
use super::SetupChoices;
use super::views::{
    build_model_options, build_voice_options, is_voicevox_connection, render_launch_voicevox_row,
    resolve_selected_option,
};
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{ButtonStyle, DropdownSpec, Tone, dropdown_select, pill_button};
use crate::ui::{GOOGLE_AGENT_PLATFORM, Hub, SettingsSection};
use ene_client_core::domain::LlmProvider;
use gpui::{AnyElement, App, Context, Entity, SharedString, div, prelude::*, px};

pub(super) fn collect_setup_choices(pod: &CompanionPod, cx: &App) -> SetupChoices {
    let hub = pod.hub.read(cx);
    let target_conn_id = hub.setup.pending_connection_id.clone().or_else(|| {
        let providers = hub.model.connections.providers.as_deref()?;
        hub.model.settings().and_then(|settings| {
            settings
                .connections
                .iter()
                .find(|c| c.can_start_chat(providers))
                .map(|c| c.id.clone())
        })
    });
    let model_options = build_model_options(hub, target_conn_id.as_deref());
    let selected_model = resolve_selected_option(pod.setup_model_id.as_deref(), &model_options);
    let checking_models = target_conn_id
        .as_deref()
        .is_some_and(|cid| hub.setup.checking_connections.contains_key(cid));

    let voicevox_conn = hub.model.settings().and_then(|settings| {
        settings
            .connections
            .iter()
            .find(|c| is_voicevox_connection(c))
    });
    let voicevox_conn_id = voicevox_conn.map(|c| c.id.clone());
    let voicevox_enabled = voicevox_conn.is_some_and(|c| c.enabled);
    let voicevox_checking = voicevox_conn_id
        .as_deref()
        .is_some_and(|cid| hub.setup.checking_connections.contains_key(cid))
        || hub.setup.checking_connections.contains_key("voicevox");
    let voice_options = build_voice_options(hub, voicevox_conn_id.as_deref());
    let selected_voice =
        resolve_selected_option(pod.setup_voice_speaker_id.as_deref(), &voice_options);

    SetupChoices {
        target_conn_id,
        model_options,
        selected_model,
        checking_models,
        voicevox_conn_id,
        voicevox_enabled,
        voicevox_checking,
        voice_options,
        selected_voice,
    }
}

pub(super) fn append_setup_model_section(
    pod: &CompanionPod,
    body: gpui::Div,
    refresh_cid: &str,
    choices: &mut SetupChoices,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    let pod_model_select = cx.weak_entity();
    let hub_refresh_models = pod.hub.clone();
    let refresh_id = refresh_cid.to_owned();
    let checking = choices.checking_models;

    body.child(
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
                    .child("使用モデル"),
            )
            .child(pill_button(
                "onboard-refresh-models",
                if checking {
                    "確認中…"
                } else {
                    "接続を確認"
                },
                Tone::Ghost.enabled(!checking),
                theme,
                move |_, cx| {
                    hub_refresh_models
                        .update(cx, |hub, cx| hub.check_connection(refresh_id.clone(), cx));
                },
            )),
    )
    .child(dropdown_select(
        DropdownSpec {
            id: "onboard-model-dropdown".to_owned(),
            selected_value: choices.selected_model.clone(),
            placeholder: if checking {
                "モデル一覧を取得中…"
            } else {
                "モデルを選択…"
            }
            .into(),
            options: std::mem::take(&mut choices.model_options),
            enabled: !checking,
            max_list_height: 168.,
            search_placeholder: "モデルを検索…",
            empty_label: "取得済みのモデルがありません",
        },
        move |val, _, cx| {
            _ = pod_model_select.update(cx, |this, cx| {
                this.setup_model_id = Some(val);
                cx.notify();
            });
        },
    ))
}

pub(super) fn append_setup_voice_section(
    pod: &CompanionPod,
    body: gpui::Div,
    choices: &mut SetupChoices,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    if !(choices.voicevox_enabled && !choices.voice_options.is_empty()) {
        return body.child(render_launch_voicevox_row(
            choices.voicevox_conn_id.clone(),
            choices.voicevox_enabled,
            choices.voicevox_checking,
            &pod.hub,
            theme,
        ));
    }

    let pod_voice_select = cx.weak_entity();
    let vv_refresh = pod.hub.clone();
    let vv_cid = choices.voicevox_conn_id.clone();
    let checking = choices.voicevox_checking;

    body.child(
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
                    .child("VOICEVOXの話者"),
            )
            .when_some(vv_cid, |row, cid| {
                row.child(pill_button(
                    "onboard-refresh-voicevox",
                    if checking {
                        "起動・取得中…".to_owned()
                    } else {
                        "再取得".to_owned()
                    },
                    Tone::Ghost.enabled(!checking),
                    theme,
                    move |_, cx| {
                        vv_refresh.update(cx, |hub, cx| hub.check_connection(cid.clone(), cx));
                    },
                ))
            }),
    )
    .child(dropdown_select(
        DropdownSpec {
            id: "onboard-voice-dropdown".to_owned(),
            selected_value: choices.selected_voice.clone(),
            placeholder: "話者を選択…".into(),
            options: std::mem::take(&mut choices.voice_options),
            enabled: !checking,
            max_list_height: 148.,
            search_placeholder: "話者を検索…",
            empty_label: "話者が見つかりません",
        },
        move |val, _, cx| {
            _ = pod_voice_select.update(cx, |this, cx| {
                this.setup_voice_speaker_id = Some(val);
                cx.notify();
            });
        },
    ))
}

pub(super) fn render_providers_loading(hub: &Entity<Hub>, theme: &Theme) -> AnyElement {
    let hub = hub.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child("接続先を読み込んでいます…")
        .child(pill_button(
            "setup-settings",
            "接続設定を開く",
            Tone::Ghost,
            theme,
            move |_, cx| {
                hub.update(cx, |hub, cx| {
                    hub.open_settings(SettingsSection::Connections, cx)
                })
            },
        ))
        .into_any_element()
}

pub(super) fn build_provider_chips(
    providers: &[LlmProvider],
    picked: Option<&LlmProvider>,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> Vec<AnyElement> {
    let mut chips = Vec::new();
    for p in providers {
        let pid = p.id.clone();
        let prov_needs_key = p.requires_credentials;
        let active = picked.is_some_and(|sel| sel.id == pid);
        let pod = cx.entity();
        chips.push(pill_button(
            SharedString::from(format!("onboard-prov-{}", p.id)),
            p.name.clone(),
            ButtonStyle::new(if active { Tone::Primary } else { Tone::Ghost })
                .maybe_icon(active.then_some(Icon::Check)),
            theme,
            move |_, cx| {
                pod.update(cx, |this, cx| {
                    if !active {
                        this.onboard_key.update(cx, crate::input::Draft::clear);
                    }
                    this.choosing_provider = false;
                    this.selected_provider_id = Some(pid.clone());
                    let hint = if prov_needs_key {
                        "APIキーを入力…"
                    } else {
                        "APIキー（任意）"
                    };
                    this.onboard_key
                        .update(cx, |d, cx| d.set_placeholder(hint, cx));
                    cx.notify();
                });
            },
        ));
    }
    chips
}

pub(super) fn provider_keys_url(id: &str) -> Option<&'static str> {
    match id {
        "openrouter" => Some("https://openrouter.ai/settings/keys"),
        "openai" => Some("https://platform.openai.com/api-keys"),
        "anthropic" => Some("https://console.anthropic.com/settings/keys"),
        "google" | "gemini" => Some("https://aistudio.google.com/apikey"),
        GOOGLE_AGENT_PLATFORM => Some("https://console.cloud.google.com/agent-platform/api-keys"),
        "sakura" => Some("https://secure.sakura.ad.jp/cloud/"),
        _ => None,
    }
}

pub(super) fn onboarding_subtitle(is_agent_platform: bool, needs_key: bool, name: &str) -> String {
    if is_agent_platform {
        "Express mode の API キーを入力してください。Cloud プロジェクトを指定する場合は詳細設定で登録できます。".to_owned()
    } else if needs_key {
        format!(
            "お使いの AI サービスを選んで API キーを入力してください（現在: {name}）。ローカル接続は詳細設定から追加できます。"
        )
    } else {
        format!("選択中の接続先（{name}）で会話を始められます。")
    }
}

pub(super) fn render_connection_header(
    hub: &Entity<Hub>,
    enabled: bool,
    theme: &Theme,
) -> gpui::Div {
    let hub_open = hub.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child("eneと話す準備"),
        )
        .child(pill_button(
            "setup-open-settings",
            "詳細設定",
            Tone::Ghost.icon(Icon::Sliders).enabled(enabled),
            theme,
            move |_, cx| {
                hub_open.update(cx, |hub, cx| {
                    hub.open_settings(SettingsSection::Connections, cx);
                });
            },
        ))
}

pub(super) fn render_provider_chip_row(
    chips: Vec<AnyElement>,
    hub: &Entity<Hub>,
    enabled: bool,
    theme: &Theme,
) -> gpui::Div {
    let hub_settings = hub.clone();
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .children(chips)
        .child(pill_button(
            "setup-local",
            "その他・ローカル接続",
            Tone::Ghost.enabled(enabled),
            theme,
            move |_, cx| {
                hub_settings.update(cx, |hub, cx| {
                    hub.open_settings(SettingsSection::Connections, cx)
                })
            },
        ))
}

pub(super) fn render_keys_link_row(
    url: &'static str,
    name: &str,
    enabled: bool,
    theme: &Theme,
) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(6.))
        .child(pill_button(
            "setup-keys",
            format!("{name} のキー取得先を開く"),
            Tone::Soft.enabled(enabled),
            theme,
            move |_, cx| cx.open_url(url),
        ))
}
