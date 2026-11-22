use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, disclosure_row, pill_button};
use crate::ui::{Hub, SearchPresetSpec, SettingsStudio};
use ene_client_core::{
    connections::build_custom_connection_input,
    domain::{ConnectionKind, LlmProvider},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

impl SettingsStudio {
    pub(super) fn render_add_connection_panel(
        &mut self,
        live: bool,
        providers: &[LlmProvider],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.connection_form.custom_connection_open
            && self.connection_form.custom_kind == ConnectionKind::Mcp
        {
            return self.render_mcp_add(live, theme, cx);
        }
        let llm_chips = build_llm_provider_chips(providers, live, &self.hub, theme, cx);
        let custom_section = self.render_custom_connection_section(live, theme, cx);
        let studio = cx.entity();
        let custom_open = self.connection_form.custom_connection_open;

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .when(!llm_chips.is_empty(), |this| {
                this.child(render_chip_section("AIプロバイダを追加", llm_chips, theme))
            })
            .child(render_chip_section(
                "音声・埋め込み機能を追加",
                build_voice_embedding_chips(live, &self.hub, theme, cx),
                theme,
            ))
            .child(render_chip_section(
                "Web 検索を追加",
                build_search_preset_chips(live, &self.hub, theme, cx),
                theme,
            ))
            .child(disclosure_row(
                "custom-connection",
                "カスタム接続",
                custom_open,
                theme,
                move |_, cx| {
                    studio.update(cx, |this, cx| {
                        this.connection_form.custom_connection_open =
                            !this.connection_form.custom_connection_open;
                        cx.notify();
                    });
                },
            ))
            .when_some(custom_section, |panel, section| panel.child(section))
            .into_any_element()
    }

    fn render_custom_connection_section(
        &mut self,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if !self.connection_form.custom_connection_open {
            return None;
        }
        let custom_kind = self.connection_form.custom_kind;
        let kind_chips = build_custom_kind_chips(custom_kind, theme, cx);
        let (adapter_label, adapter_placeholder, target_label, target_placeholder) =
            custom_kind_placeholders(custom_kind);

        let name_d = self.draft(
            cx,
            "add:custom:name",
            "表示名（例: ローカル LLM）",
            None,
            theme,
        );
        let adapter_d = self.draft(cx, "add:custom:adapter", adapter_placeholder, None, theme);
        adapter_d.update(cx, |d, cx| d.set_placeholder(adapter_placeholder, cx));
        let target_d = self.draft(cx, "add:custom:target", target_placeholder, None, theme);
        target_d.update(cx, |d, cx| d.set_placeholder(target_placeholder, cx));

        let c_name = name_d.read(cx).text().trim().to_owned();
        let c_adapter = adapter_d.read(cx).text().trim().to_owned();
        let c_target = target_d.read(cx).text().trim().to_owned();
        let hub_custom = self.hub.clone();
        let studio_done = cx.entity();

        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .children(kind_chips),
                )
                .child(labeled_field("名前", name_d, theme))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(10.))
                        .child(div().flex_1().min_w_0().child(labeled_field(
                            adapter_label,
                            adapter_d,
                            theme,
                        )))
                        .child(div().flex_1().min_w_0().child(labeled_field(
                            target_label,
                            target_d,
                            theme,
                        ))),
                )
                .child(
                    div().flex().flex_row().justify_end().child(pill_button(
                        "add-custom-submit",
                        "追加",
                        Tone::Primary
                            .icon(Icon::Plus)
                            .enabled(live && !c_name.is_empty()),
                        theme,
                        move |_, cx| {
                            let Some(input) = build_custom_connection_input(
                                custom_kind,
                                &c_name,
                                &c_adapter,
                                &c_target,
                            ) else {
                                return;
                            };
                            hub_custom.update(cx, |hub, cx| {
                                hub.upsert_connection("接続の追加", input, cx);
                            });
                            studio_done.update(cx, |this, cx| {
                                this.connection_form.adding_connection = false;
                                cx.notify();
                            });
                        },
                    )),
                ),
        )
    }
}

fn render_chip_section(title: &'static str, chips: Vec<AnyElement>, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child(title),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.))
                .children(chips),
        )
}

fn build_llm_provider_chips(
    providers: &[LlmProvider],
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> Vec<AnyElement> {
    providers
        .iter()
        .map(|prov| {
            let p = prov.clone();
            let hub = hub.clone();
            let studio = cx.entity();
            pill_button(
                SharedString::from(format!("add-llm-prov-{}", prov.id)),
                prov.name.clone(),
                Tone::Soft.icon(Icon::Plus).enabled(live),
                theme,
                move |_, cx| {
                    hub.update(cx, |hub, cx| {
                        hub.add_llm_from_provider(p.clone(), vec![], cx)
                    });
                    studio.update(cx, |this, cx| {
                        this.connection_form.adding_connection = false;
                        cx.notify();
                    });
                },
            )
        })
        .collect()
}

struct PresetChipSpec {
    id: &'static str,
    label: &'static str,
    style: crate::ui::widgets::ButtonStyle,
    expand_id: Option<&'static str>,
    action: fn(&mut Hub, &mut Context<Hub>),
}

fn build_voice_embedding_chips(
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> Vec<AnyElement> {
    let specs = [
        PresetChipSpec {
            id: "add-preset-voicevox",
            label: "VOICEVOX（ローカル）",
            style: Tone::Mint.icon(Icon::Volume).enabled(live),
            expand_id: Some("voicevox"),
            action: |hub, cx| hub.add_voicevox_preset(3, None, cx),
        },
        PresetChipSpec {
            id: "add-preset-sakura-tts",
            label: "さくらのAI Engine TTS",
            style: Tone::Soft.icon(Icon::Volume).enabled(live),
            expand_id: Some("sakura-tts"),
            action: |hub, cx| hub.add_sakura_tts_preset(cx),
        },
        PresetChipSpec {
            id: "add-preset-openai-tts",
            label: "OpenAI互換 TTS",
            style: Tone::Ghost.icon(Icon::Volume).enabled(live),
            expand_id: None,
            action: |hub, cx| {
                hub.add_openai_tts_preset(
                    "https://api.openai.com/v1".into(),
                    "tts-1".into(),
                    "alloy".into(),
                    cx,
                );
            },
        },
        PresetChipSpec {
            id: "add-preset-sakura-stt",
            label: "さくらのAI Engine STT",
            style: Tone::Soft.icon(Icon::Mic).enabled(live),
            expand_id: Some("sakura-stt"),
            action: |hub, cx| hub.add_sakura_stt_preset(cx),
        },
        PresetChipSpec {
            id: "add-preset-openai-stt",
            label: "OpenAI 音声認識（STT）",
            style: Tone::Ghost.icon(Icon::Mic).enabled(live),
            expand_id: None,
            action: |hub, cx| {
                hub.add_stt_preset(
                    "OpenAI 音声認識".into(),
                    "https://api.openai.com/v1".into(),
                    "gpt-4o-mini-transcribe".into(),
                    "ja".into(),
                    cx,
                );
            },
        },
        PresetChipSpec {
            id: "add-preset-whisper-stt",
            label: "Whisper STT（ローカル）",
            style: Tone::Ghost.icon(Icon::Mic).enabled(live),
            expand_id: None,
            action: |hub, cx| {
                hub.add_stt_preset(
                    "ローカル Whisper".into(),
                    "http://127.0.0.1:8080/v1".into(),
                    "whisper-1".into(),
                    "ja".into(),
                    cx,
                );
            },
        },
        PresetChipSpec {
            id: "add-preset-sakura-embedding",
            label: "さくらのAI Engine Embedding",
            style: Tone::Soft.icon(Icon::Search).enabled(live),
            expand_id: Some("sakura-embedding"),
            action: |hub, cx| hub.add_sakura_embedding_preset(cx),
        },
        PresetChipSpec {
            id: "add-preset-openai-embedding",
            label: "OpenAI Embedding",
            style: Tone::Ghost.icon(Icon::Search).enabled(live),
            expand_id: Some("openai-embedding"),
            action: |hub, cx| hub.add_openai_embedding_preset(cx),
        },
    ];
    specs
        .into_iter()
        .map(|spec| preset_chip(spec, hub, theme, cx))
        .collect()
}

fn preset_chip(
    spec: PresetChipSpec,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> AnyElement {
    let hub = hub.clone();
    let studio = cx.entity();
    let expand_id = spec.expand_id;
    let action = spec.action;
    pill_button(spec.id, spec.label, spec.style, theme, move |_, cx| {
        hub.update(cx, action);
        studio.update(cx, |this, cx| {
            this.connection_form.adding_connection = false;
            if let Some(eid) = expand_id {
                this.connection_form.expanded_connection_id = Some(eid.into());
                this.open_dropdown = None;
            }
            cx.notify();
        });
    })
}

fn build_search_preset_chips(
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> Vec<AnyElement> {
    let search_presets = [
        (
            "add-preset-search-exa",
            "search-exa",
            "Exa（APIキー）",
            "Web検索 (Exa)",
            "exa",
            "https://api.exa.ai/search",
            Tone::Soft,
        ),
        (
            "add-preset-search-tavily",
            "search-tavily",
            "Tavily（APIキー）",
            "Web検索 (Tavily)",
            "tavily",
            "https://api.tavily.com/search",
            Tone::Soft,
        ),
        (
            "add-preset-search-brave",
            "search-brave",
            "Brave Search（APIキー）",
            "Web検索 (Brave)",
            "brave",
            "https://api.search.brave.com/res/v1/web/search",
            Tone::Ghost,
        ),
        (
            "add-preset-search-mwmbl",
            "search-mwmbl",
            "Mwmbl（キー不要）",
            "Web検索 (Mwmbl)",
            "mwmbl",
            "https://api.mwmbl.org/api/v2/search/",
            Tone::Mint,
        ),
        (
            "add-preset-search-searxng",
            "search-searxng",
            "SearXNG（自前・ローカル）",
            "Web検索 (SearXNG)",
            "searxng",
            "http://127.0.0.1:8080/search",
            Tone::Ghost,
        ),
    ];
    search_presets
        .into_iter()
        .map(
            |(btn_id, preset_id, label, conn_name, adapter, url, tone)| {
                let hub_s = hub.clone();
                let studio_s = cx.entity();
                pill_button(
                    btn_id,
                    label,
                    tone.icon(Icon::Search).enabled(live),
                    theme,
                    move |_, cx| {
                        let target_id = hub_s.update(cx, |hub, cx| {
                            hub.add_search_preset(
                                SearchPresetSpec {
                                    preset_id,
                                    name: conn_name,
                                    adapter,
                                    endpoint: url,
                                    activate: true,
                                },
                                cx,
                            )
                        });
                        studio_s.update(cx, |this, cx| {
                            this.connection_form.adding_connection = false;
                            this.connection_form.expanded_connection_id = Some(target_id);
                            this.open_dropdown = None;
                            cx.notify();
                        });
                    },
                )
            },
        )
        .collect()
}

fn build_custom_kind_chips(
    custom_kind: ConnectionKind,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> Vec<AnyElement> {
    ConnectionKind::ALL_CUSTOM
        .into_iter()
        .map(|kind| {
            let selected = custom_kind == kind;
            let studio = cx.entity();
            pill_button(
                SharedString::from(format!("add-custom-kind-{kind:?}")),
                kind.label(),
                if selected { Tone::Primary } else { Tone::Ghost },
                theme,
                move |_, cx| {
                    studio.update(cx, |this, cx| {
                        this.connection_form.custom_kind = kind;
                        cx.notify();
                    });
                },
            )
        })
        .collect()
}

fn custom_kind_placeholders(
    kind: ConnectionKind,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match kind {
        ConnectionKind::Search => (
            "検索アダプタ",
            "例: exa / tavily / brave / mwmbl / searxng",
            "検索 API URL",
            "例: https://api.exa.ai/search",
        ),
        ConnectionKind::Acp | ConnectionKind::Mcp => (
            "実行コマンド",
            "例: npx / python / my-agent",
            "引数（スペース区切り）",
            "例: --stdio --workspace .",
        ),
        _ => (
            "アダプタ名",
            "例: openai / voicevox / whisper",
            "エンドポイント URL",
            "例: http://127.0.0.1:11434/v1",
        ),
    }
}
