use super::CompanionPod;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    ButtonStyle, DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button,
};
use crate::ui::{GOOGLE_AGENT_PLATFORM, Hub, NewCompanionSpec, SettingsSection};
use ene_client_core::domain::{Connection, ConnectionConfig, ConnectionKind, LlmProvider};
use gpui::{AnyElement, App, Context, Entity, SharedString, div, prelude::*, px};

struct SetupChoices {
    target_conn_id: Option<String>,
    model_options: Vec<DropdownOption>,
    selected_model: Option<String>,
    selected_model_label: Option<String>,
    checking_models: bool,
    voicevox_conn_id: Option<String>,
    voicevox_enabled: bool,
    voicevox_checking: bool,
    voice_options: Vec<DropdownOption>,
    selected_voice: Option<String>,
    selected_voice_label: Option<String>,
}

pub(super) fn render_onboarding_body(
    pod: &CompanionPod,
    needs_connection: bool,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    if needs_connection {
        return render_connection_onboarding(pod, theme, cx);
    }
    render_model_voice_onboarding(pod, theme, cx)
}

fn render_connection_onboarding(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let providers = pod
        .hub
        .read(cx)
        .model
        .connections
        .providers
        .clone()
        .unwrap_or_default();
    if providers.is_empty() {
        return render_providers_loading(&pod.hub, theme);
    }
    let picked = pod
        .selected_provider_id
        .as_deref()
        .and_then(|id| providers.iter().find(|p| p.id == id))
        .or_else(|| providers.iter().find(|p| p.id == "openrouter"))
        .or_else(|| providers.first())
        .cloned();
    let provider_chips = build_provider_chips(&providers, picked.as_ref(), theme, cx);

    let look = theme.field_look(false, false);
    pod.onboard_key.update(cx, |d, _| d.apply_look(look));
    let secret = pod.onboard_key.read(cx).text().trim().to_owned();
    let needs_key = picked.as_ref().is_some_and(|p| p.requires_credentials);
    let can_connect = picked.is_some() && (!needs_key || !secret.is_empty());

    let hub = pod.hub.read(cx);
    let pending = hub.setup.pending;
    let err = hub.setup.error;
    let err_detail = hub.setup.error_detail.clone();
    let name = picked
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "AIサービス".to_owned());
    let is_agent_platform = picked
        .as_ref()
        .is_some_and(|p| p.adapter == GOOGLE_AGENT_PLATFORM);
    let keys_url = picked.as_ref().and_then(|p| provider_keys_url(&p.id));
    let subtitle = onboarding_subtitle(is_agent_platform, needs_key, &name);

    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(render_connection_header(&pod.hub, !pending, theme))
        .child(div().text_sm().child(subtitle))
        .child(render_provider_chip_row(
            provider_chips,
            &pod.hub,
            !pending,
            theme,
        ))
        .when_some(keys_url, |this, url| {
            this.child(render_keys_link_row(url, &name, !pending, theme))
        })
        .child(pod.onboard_key.clone())
        .when_some(err, |row, msg| {
            row.child(div().text_xs().text_color(theme.danger_fg).child(msg))
        })
        .when_some(err_detail, |row, detail| {
            row.child(div().text_xs().text_color(theme.danger_fg).child(detail))
        })
        .child(render_connect_submit_row(
            ConnectSubmitSpec {
                picked,
                name: &name,
                secret,
                can_connect,
                pending,
            },
            &pod.hub,
            theme,
            cx,
        ))
        .into_any_element()
}

fn render_model_voice_onboarding(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let look = theme.field_look(false, false);
    pod.onboard_name.update(cx, |d, _| d.apply_look(look));
    pod.setup_model_filter.update(cx, |d, _| d.apply_look(look));
    pod.setup_voice_filter.update(cx, |d, _| d.apply_look(look));
    let companions_empty = pod
        .hub
        .read(cx)
        .model
        .snapshot()
        .is_none_or(|snap| snap.companions.is_empty());
    let choices = collect_setup_choices(pod, cx);

    let name = pod.onboard_name.read(cx).text().trim().to_owned();
    let can_finish = (!companions_empty || !name.is_empty())
        && ((choices.target_conn_id.is_some() && choices.selected_model.is_some())
            || (companions_empty && choices.selected_model.is_none() && !choices.checking_models));

    let mut body = div().flex().flex_col().gap(px(8.)).child(
        div()
            .font_weight(gpui::FontWeight::BOLD)
            .child("使うモデルと音声を選択"),
    );

    if let Some(refresh_cid) = choices.target_conn_id.clone() {
        body = append_setup_model_section(pod, body, &refresh_cid, &choices, theme, cx);
    }
    body = append_setup_voice_section(pod, body, &choices, theme, cx);
    if companions_empty {
        body = body
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("コンパニオンの名前"),
            )
            .child(pod.onboard_name.clone());
    }

    let finish_ctx = SetupFinishParams {
        companions_empty,
        name,
        conn_id: choices.target_conn_id,
        model_id: choices.selected_model,
        voice_id: choices.selected_voice,
        can_finish,
    };
    body.child(render_setup_finish_button(finish_ctx, &pod.hub, theme))
        .into_any_element()
}

fn collect_setup_choices(pod: &CompanionPod, cx: &App) -> SetupChoices {
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
    let selected_model_label = find_option_label(&model_options, selected_model.as_deref());
    let checking_models = target_conn_id
        .as_deref()
        .is_some_and(|cid| hub.setup.checking_connections.contains(cid));

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
        .is_some_and(|cid| hub.setup.checking_connections.contains(cid))
        || hub.setup.checking_connections.contains("voicevox");
    let voice_options = build_voice_options(hub, voicevox_conn_id.as_deref());
    let selected_voice =
        resolve_selected_option(pod.setup_voice_speaker_id.as_deref(), &voice_options);
    let selected_voice_label = find_option_label(&voice_options, selected_voice.as_deref());

    SetupChoices {
        target_conn_id,
        model_options,
        selected_model,
        selected_model_label,
        checking_models,
        voicevox_conn_id,
        voicevox_enabled,
        voicevox_checking,
        voice_options,
        selected_voice,
        selected_voice_label,
    }
}

fn append_setup_model_section(
    pod: &CompanionPod,
    body: gpui::Div,
    refresh_cid: &str,
    choices: &SetupChoices,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    let model_filter_text = pod.setup_model_filter.read(cx).text().to_owned();
    let pod_model_toggle = cx.entity();
    let pod_model_select = cx.entity();
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
                    "取得中…".to_owned()
                } else {
                    "再取得".to_owned()
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
            selected_value: choices.selected_model.as_deref(),
            selected_label: choices.selected_model_label.as_deref(),
            placeholder: if checking {
                "モデル一覧を取得中…"
            } else {
                "モデルを選択…"
            },
            options: &choices.model_options,
            open: pod.setup_model_dropdown_open,
            enabled: !checking,
            max_list_height: 168.,
            filter_draft: Some(pod.setup_model_filter.clone()),
            filter_text: &model_filter_text,
            empty_label: "取得済みのモデルがありません（「再取得」で確認）",
        },
        theme,
        move |_, cx| {
            pod_model_toggle.update(cx, |this, cx| {
                this.setup_model_dropdown_open = !this.setup_model_dropdown_open;
                if this.setup_model_dropdown_open {
                    this.setup_voice_dropdown_open = false;
                }
                cx.notify();
            });
        },
        move |val, _, cx| {
            pod_model_select.update(cx, |this, cx| {
                this.setup_model_id = Some(val.clone());
                this.setup_model_dropdown_open = false;
                this.setup_model_filter.update(cx, |d, cx| d.clear(cx));
                cx.notify();
            });
        },
    ))
}

fn append_setup_voice_section(
    pod: &CompanionPod,
    body: gpui::Div,
    choices: &SetupChoices,
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

    let voice_filter_text = pod.setup_voice_filter.read(cx).text().to_owned();
    let pod_voice_toggle = cx.entity();
    let pod_voice_select = cx.entity();
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
            selected_value: choices.selected_voice.as_deref(),
            selected_label: choices.selected_voice_label.as_deref(),
            placeholder: "話者を選択…",
            options: &choices.voice_options,
            open: pod.setup_voice_dropdown_open,
            enabled: !checking,
            max_list_height: 148.,
            filter_draft: Some(pod.setup_voice_filter.clone()),
            filter_text: &voice_filter_text,
            empty_label: "話者が見つかりません",
        },
        theme,
        move |_, cx| {
            pod_voice_toggle.update(cx, |this, cx| {
                this.setup_voice_dropdown_open = !this.setup_voice_dropdown_open;
                if this.setup_voice_dropdown_open {
                    this.setup_model_dropdown_open = false;
                }
                cx.notify();
            });
        },
        move |val, _, cx| {
            pod_voice_select.update(cx, |this, cx| {
                this.setup_voice_speaker_id = Some(val.clone());
                this.setup_voice_dropdown_open = false;
                this.setup_voice_filter.update(cx, |d, cx| d.clear(cx));
                cx.notify();
            });
        },
    ))
}

fn render_providers_loading(hub: &Entity<Hub>, theme: &Theme) -> AnyElement {
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

fn build_provider_chips(
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
                        this.onboard_key.update(cx, |d, cx| d.clear(cx));
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

fn provider_keys_url(id: &str) -> Option<&'static str> {
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

fn onboarding_subtitle(is_agent_platform: bool, needs_key: bool, name: &str) -> String {
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

fn render_connection_header(hub: &Entity<Hub>, enabled: bool, theme: &Theme) -> gpui::Div {
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

fn render_provider_chip_row(
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

fn render_keys_link_row(url: &'static str, name: &str, enabled: bool, theme: &Theme) -> gpui::Div {
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

struct ConnectSubmitSpec<'a> {
    picked: Option<LlmProvider>,
    name: &'a str,
    secret: String,
    can_connect: bool,
    pending: bool,
}

fn render_connect_submit_row(
    spec: ConnectSubmitSpec<'_>,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    let hub_connect = hub.clone();
    let pod = cx.entity();
    let picked = spec.picked;
    let secret = spec.secret;
    div().flex().justify_end().child(pill_button(
        "onboard-connect-llm",
        if spec.pending {
            "接続してモデルを取得中…".to_owned()
        } else {
            format!("{}に接続してモデルを取得", spec.name)
        },
        Tone::Primary
            .icon(Icon::Check)
            .enabled(spec.can_connect && !spec.pending),
        theme,
        move |_, cx| {
            if let Some(provider) = picked.clone() {
                hub_connect.update(cx, |hub, cx| {
                    hub.quick_connect_llm(provider, secret.clone(), cx)
                });
                pod.update(cx, |this, cx| {
                    this.onboard_key.update(cx, |d, cx| d.clear(cx));
                    cx.notify();
                });
            }
        },
    ))
}

fn is_voicevox_connection(c: &Connection) -> bool {
    c.kind == ConnectionKind::Tts
        && (c.id == "voicevox"
            || matches!(
                c.config.as_ref(),
                Some(ConnectionConfig::Tts(tts))
                    if tts
                        .endpoint
                        .as_ref()
                        .is_some_and(|ep| ep.adapter == "voicevox")
            ))
}

fn build_model_options(hub: &Hub, target_conn_id: Option<&str>) -> Vec<DropdownOption> {
    let mut pairs = target_conn_id
        .map(|cid| hub.available_models_for_connection(cid))
        .unwrap_or_default();
    for model_id in &hub.setup.pending_models {
        if !pairs.iter().any(|(id, _)| id == model_id) {
            pairs.push((model_id.clone(), model_id.clone()));
        }
    }
    pairs
        .into_iter()
        .map(|(id, label)| {
            let detail = (label != id).then(|| id.clone());
            DropdownOption {
                value: id,
                label,
                detail,
            }
        })
        .collect()
}

fn build_voice_options(hub: &Hub, voicevox_conn_id: Option<&str>) -> Vec<DropdownOption> {
    voicevox_conn_id
        .map(|cid| hub.voice_choices_for_connection(cid))
        .unwrap_or_default()
        .into_iter()
        .map(|(vid, label)| {
            let detail = (label != vid).then(|| format!("話者ID: {vid}"));
            DropdownOption {
                value: vid,
                label,
                detail,
            }
        })
        .collect()
}

fn resolve_selected_option(current: Option<&str>, options: &[DropdownOption]) -> Option<String> {
    current
        .filter(|id| options.iter().any(|opt| opt.value == *id))
        .or_else(|| options.first().map(|opt| opt.value.as_str()))
        .map(str::to_owned)
}

fn find_option_label(options: &[DropdownOption], selected: Option<&str>) -> Option<String> {
    selected.and_then(|id| {
        options
            .iter()
            .find(|opt| opt.value == id)
            .map(|opt| opt.label.clone())
    })
}

fn render_launch_voicevox_row(
    vv_id: Option<String>,
    voicevox_enabled: bool,
    voicevox_checking: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_voicevox = hub.clone();
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
                .child("VOICEVOX音声（任意）"),
        )
        .child(pill_button(
            "onboard-launch-voicevox",
            if voicevox_checking {
                "VOICEVOXを起動中…".to_owned()
            } else {
                "VOICEVOXを起動して使う".to_owned()
            },
            Tone::Soft.icon(Icon::Volume).enabled(!voicevox_checking),
            theme,
            move |_, cx| {
                let cid = vv_id.clone();
                hub_voicevox.update(cx, |hub, cx| {
                    if voicevox_enabled {
                        if let Some(id) = cid {
                            hub.check_connection(id, cx);
                        }
                    } else {
                        hub.add_voicevox_preset(3, None, cx);
                    }
                });
            },
        ))
}

struct SetupFinishParams {
    companions_empty: bool,
    name: String,
    conn_id: Option<String>,
    model_id: Option<String>,
    voice_id: Option<String>,
    can_finish: bool,
}

fn render_setup_finish_button(
    params: SetupFinishParams,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_start = hub.clone();
    let label = if params.model_id.is_some() {
        "このモデルで始める"
    } else {
        "対話を始める"
    };
    div().flex().flex_row().justify_end().child(pill_button(
        "onboard-finish",
        label,
        Tone::Primary
            .icon(Icon::Sparkles)
            .enabled(params.can_finish),
        theme,
        move |_, cx| {
            let name_arg = params.companions_empty.then(|| params.name.clone());
            let voice_arg = params.voice_id.clone();
            if let (Some(cid), Some(mid)) = (params.conn_id.clone(), params.model_id.clone()) {
                hub_start.update(cx, |hub, cx| {
                    hub.complete_setup_selection(cid, mid, name_arg, voice_arg, cx);
                });
            } else if params.companions_empty && !params.name.is_empty() {
                hub_start.update(cx, |hub, cx| {
                    hub.create_companion(
                        NewCompanionSpec {
                            name: params.name.clone(),
                            persona: "柔らかく親しみやすい口調で会話します。".into(),
                            mind: "相手を思いやりながら率直に対話する。".into(),
                            voice_identifier: voice_arg,
                        },
                        cx,
                    );
                });
            }
        },
    ))
}
