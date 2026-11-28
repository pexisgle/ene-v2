//! The connect-submit row, voice launch and finish button.

use crate::input::Draft;
use crate::ui::hub::companions as companion_actions;
use crate::ui::hub::presets;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, Tone, pill_button};
use crate::ui::{Hub, NewCompanionSpec};
use ene_client_core::domain::{Connection, ConnectionConfig, ConnectionKind, LlmProvider};
use gpui::component::StyledExt as _;
use gpui::{Entity, div, prelude::*, px};

pub(super) struct ConnectSubmitSpec {
    pub(super) picked: Option<LlmProvider>,
    pub(super) can_connect: bool,
    pub(super) pending: bool,
}

pub(super) fn render_connect_submit_row(
    spec: ConnectSubmitSpec,
    hub: &Entity<Hub>,
    key: &Entity<Draft>,
    theme: &Theme,
) -> gpui::Div {
    let hub_connect = hub.clone();
    let key = key.clone();
    let picked = spec.picked;
    div().flex().justify_end().child(pill_button(
        "onboard-connect-llm",
        if spec.pending {
            "接続設定を保存中…"
        } else {
            "接続設定を保存"
        },
        Tone::Primary
            .icon(Icon::Check)
            .enabled(spec.can_connect && !spec.pending),
        theme,
        move |_, cx| {
            let Some(provider) = picked.clone() else {
                return;
            };
            let secret = key.read(cx).text().trim().to_owned();
            if hub_connect.update(cx, |hub, cx| {
                presets::save_setup_connection(hub, provider, secret, cx)
            }) {
                key.update(cx, Draft::clear);
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
                    if tts.endpoint.adapter == "voicevox"
            ))
}

pub(super) fn build_model_options(hub: &Hub, target_conn_id: Option<&str>) -> Vec<DropdownOption> {
    target_conn_id
        .map_or_default(|cid| hub.model.available_models_for_connection(cid))
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
        .map_or_default(|cid| hub.model.voice_choices_for_connection(cid))
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

pub(super) fn render_launch_voicevox_row(
    vv_id: Option<String>,
    voicevox_enabled: bool,
    voicevox_checking: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_voicevox = hub.clone();
    div()
        .h_flex()
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
            Tone::Soft.icon(Icon::Volume2).enabled(!voicevox_checking),
            theme,
            move |_, cx| {
                let cid = vv_id.clone();
                hub_voicevox.update(cx, |hub, cx| {
                    if voicevox_enabled {
                        if let Some(id) = cid {
                            hub.check_connection(id, cx);
                        }
                    } else {
                        presets::add_featured_preset(
                            hub,
                            ene_client_core::presets::FeaturedPreset::Voicevox,
                            cx,
                        );
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
    pub(super) applying: bool,
}

pub(super) fn render_setup_finish_button(
    params: SetupFinishParams,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_start = hub.clone();
    let label = if params.applying {
        "モデルを割り当て中…"
    } else if params.model_id.is_some() {
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
                    presets::complete_setup_selection(hub, cid, &mid, name_arg, voice_arg, cx);
                });
            } else if params.companions_empty && !params.name.is_empty() {
                hub_start.update(cx, |hub, cx| {
                    companion_actions::create_companion(
                        hub,
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
