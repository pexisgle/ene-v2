//! The connect-submit row, voice launch and finish button.

use super::CompanionPod;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, Tone, pill_button};
use crate::ui::{Hub, NewCompanionSpec};
use ene_client_core::domain::{Connection, ConnectionConfig, ConnectionKind, LlmProvider};
use gpui::{Context, Entity, div, prelude::*, px};

pub(super) struct ConnectSubmitSpec<'a> {
    pub(super) picked: Option<LlmProvider>,
    pub(super) name: &'a str,
    pub(super) secret: String,
    pub(super) can_connect: bool,
    pub(super) pending: bool,
}

pub(super) fn render_connect_submit_row(
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
                    this.onboard_key.update(cx, crate::input::Draft::clear);
                    cx.notify();
                });
            }
        },
    ))
}

pub(super) fn is_voicevox_connection(c: &Connection) -> bool {
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

pub(super) fn build_model_options(hub: &Hub, target_conn_id: Option<&str>) -> Vec<DropdownOption> {
    let mut pairs = target_conn_id.map_or_default(|cid| hub.available_models_for_connection(cid));
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

pub(super) fn build_voice_options(
    hub: &Hub,
    voicevox_conn_id: Option<&str>,
) -> Vec<DropdownOption> {
    voicevox_conn_id
        .map_or_default(|cid| hub.voice_choices_for_connection(cid))
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

pub(super) fn resolve_selected_option(
    current: Option<&str>,
    options: &[DropdownOption],
) -> Option<String> {
    current
        .filter(|id| options.iter().any(|opt| opt.value == *id))
        .or_else(|| options.first().map(|opt| opt.value.as_str()))
        .map(str::to_owned)
}

pub(super) fn find_option_label(
    options: &[DropdownOption],
    selected: Option<&str>,
) -> Option<String> {
    selected.and_then(|id| {
        options
            .iter()
            .find(|opt| opt.value == id)
            .map(|opt| opt.label.clone())
    })
}

pub(super) fn render_launch_voicevox_row(
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

pub(super) struct SetupFinishParams {
    pub(super) companions_empty: bool,
    pub(super) name: String,
    pub(super) conn_id: Option<String>,
    pub(super) model_id: Option<String>,
    pub(super) voice_id: Option<String>,
    pub(super) can_finish: bool,
}

pub(super) fn render_setup_finish_button(
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
                    hub.complete_setup_selection(cid, &mid, name_arg, voice_arg, cx);
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
