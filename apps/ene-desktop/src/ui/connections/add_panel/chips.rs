//! Preset chip builders for the add-connection panel: the voice, STT,
//! embedding and web-search presets, plus the custom-connection kind picker.

use super::AddFlow;
use super::ConnectionsPane;
use crate::ui::Hub;
use crate::ui::hub::presets;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button};
use ene_client_core::connections::build_custom_connection_input;
use ene_client_core::{SearchAdapter, domain::ConnectionKind, presets::FeaturedPreset};
use gpui::{AnyElement, Context, Entity, SharedString};

#[derive(Clone, Copy)]
struct PresetChipSpec {
    id: &'static str,
    label: &'static str,
    style: crate::ui::widgets::ButtonStyle,
    expand_id: Option<&'static str>,
    preset: FeaturedPreset,
}

pub(super) fn build_voice_embedding_chips(
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> Vec<AnyElement> {
    let specs = [
        PresetChipSpec {
            id: "add-preset-voicevox",
            label: "VOICEVOX（ローカル）",
            style: Tone::Mint.icon(Icon::Volume2).enabled(live),
            expand_id: Some("voicevox"),
            preset: FeaturedPreset::Voicevox,
        },
        PresetChipSpec {
            id: "add-preset-sakura-tts",
            label: "さくらのAI Engine TTS",
            style: Tone::Soft.icon(Icon::Volume2).enabled(live),
            expand_id: Some("sakura-tts"),
            preset: FeaturedPreset::SakuraTts,
        },
        PresetChipSpec {
            id: "add-preset-openai-tts",
            label: "OpenAI互換 TTS",
            style: Tone::Ghost.icon(Icon::Volume2).enabled(live),
            expand_id: None,
            preset: FeaturedPreset::OpenAiTts,
        },
        PresetChipSpec {
            id: "add-preset-sakura-stt",
            label: "さくらのAI Engine STT",
            style: Tone::Soft.icon(Icon::Mic).enabled(live),
            expand_id: Some("sakura-stt"),
            preset: FeaturedPreset::SakuraStt,
        },
        PresetChipSpec {
            id: "add-preset-openai-stt",
            label: "OpenAI 音声認識（STT）",
            style: Tone::Ghost.icon(Icon::Mic).enabled(live),
            expand_id: None,
            preset: FeaturedPreset::OpenAiStt,
        },
        PresetChipSpec {
            id: "add-preset-whisper-stt",
            label: "Whisper STT（ローカル）",
            style: Tone::Ghost.icon(Icon::Mic).enabled(live),
            expand_id: None,
            preset: FeaturedPreset::WhisperStt,
        },
        PresetChipSpec {
            id: "add-preset-sakura-embedding",
            label: "さくらのAI Engine Embedding",
            style: Tone::Soft.icon(Icon::Search).enabled(live),
            expand_id: Some("sakura-embedding"),
            preset: FeaturedPreset::SakuraEmbedding,
        },
        PresetChipSpec {
            id: "add-preset-openai-embedding",
            label: "OpenAI Embedding",
            style: Tone::Ghost.icon(Icon::Search).enabled(live),
            expand_id: Some("openai-embedding"),
            preset: FeaturedPreset::OpenAiEmbedding,
        },
    ];
    specs
        .into_iter()
        .map(|spec| preset_chip(&spec, hub, theme, cx))
        .collect()
}

fn preset_chip(
    spec: &PresetChipSpec,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> AnyElement {
    let hub = hub.clone();
    let pane = cx.entity();
    let expand_id = spec.expand_id;
    let preset = spec.preset;
    pill_button(spec.id, spec.label, spec.style, theme, move |_, cx| {
        hub.update(cx, |hub, cx| presets::add_featured_preset(hub, preset, cx));
        pane.update(cx, |this, cx| {
            this.form.add = AddFlow::Closed;
            if let Some(eid) = expand_id {
                this.form.open_editor(eid.into());
            }
            cx.notify();
        });
    })
}

pub(in crate::ui::connections) fn build_search_preset_chips(
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> Vec<AnyElement> {
    [
        (SearchAdapter::Exa, "Exa（APIキー）", Tone::Soft),
        (SearchAdapter::Tavily, "Tavily（APIキー）", Tone::Soft),
        (SearchAdapter::Brave, "Brave Search（APIキー）", Tone::Ghost),
        (SearchAdapter::Mwmbl, "Mwmbl（キー不要）", Tone::Mint),
        (
            SearchAdapter::Searxng,
            "SearXNG（自前・ローカル）",
            Tone::Ghost,
        ),
    ]
    .into_iter()
    .map(|(adapter, label, tone)| {
        let provider = adapter.provider();
        let button = SharedString::from(format!("add-preset-{}", provider.preset_id));
        let hub_s = hub.clone();
        let pane_s = cx.entity();
        pill_button(
            button,
            label,
            tone.icon(Icon::Search).enabled(live),
            theme,
            move |_, cx| {
                let target_id =
                    hub_s.update(cx, |hub, cx| presets::add_search_preset(hub, adapter, cx));
                pane_s.update(cx, |this, cx| {
                    this.form.add = AddFlow::Closed;
                    this.form.open_editor(target_id);
                    cx.notify();
                });
            },
        )
    })
    .collect()
}

pub(super) fn build_custom_kind_chips(
    custom_kind: ConnectionKind,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> Vec<AnyElement> {
    ConnectionKind::ALL_CUSTOM
        .into_iter()
        .map(|kind| {
            let selected = custom_kind == kind;
            let pane = cx.entity();
            pill_button(
                SharedString::from(format!("add-custom-kind-{kind:?}")),
                kind.label(),
                if selected { Tone::Primary } else { Tone::Ghost },
                theme,
                move |_, cx| {
                    pane.update(cx, |this, cx| {
                        this.form.add = if kind == ConnectionKind::Mcp {
                            AddFlow::Mcp(crate::ui::connections::mcp_add::Form::default())
                        } else {
                            AddFlow::Custom(kind)
                        };
                        cx.notify();
                    });
                },
            )
        })
        .collect()
}

pub(super) fn build_harness_chips(
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> Vec<AnyElement> {
    let model = &hub.read(cx).model;
    model
        .connections
        .plugins
        .iter()
        .filter(|plugin| {
            plugin.offers_harness()
                && !model.settings().is_some_and(|settings| {
                    settings.connections.iter().any(|connection| {
                        connection.kind == ConnectionKind::Acp
                            && connection
                                .endpoint()
                                .is_some_and(|endpoint| endpoint.adapter == plugin.id)
                    })
                })
        })
        .map(|plugin| {
            let (id, name) = (plugin.id.clone(), plugin.name.clone());
            let hub = hub.clone();
            let pane = cx.entity();
            pill_button(
                SharedString::from(format!("add-harness-{id}")),
                name.clone(),
                Tone::Soft.icon(Icon::Plus).enabled(live),
                theme,
                move |_, cx| {
                    let Some(input) =
                        build_custom_connection_input(ConnectionKind::Acp, &name, &id, "")
                    else {
                        return;
                    };
                    let connection_id = input.id.clone();
                    hub.update(cx, |hub, cx| {
                        hub.upsert_connection("ハーネスの追加", input, cx)
                    });
                    pane.update(cx, |pane, cx| {
                        pane.form.add = AddFlow::Closed;
                        pane.form.open_editor(connection_id);
                        cx.notify();
                    });
                },
            )
        })
        .collect()
}
