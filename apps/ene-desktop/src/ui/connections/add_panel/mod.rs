mod chips;

use super::ConnectionsPane;
use crate::ui::Hub;
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, disclosure_row, pill_button};
use ene_client_core::{
    connections::build_custom_connection_input,
    domain::{ConnectionKind, LlmProvider},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

use chips::{build_custom_kind_chips, build_search_preset_chips, build_voice_embedding_chips};

pub(super) fn render_add_connection_panel(
    pane: &mut ConnectionsPane,
    live: bool,
    providers: &[LlmProvider],
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    if pane.form.custom_connection_open && pane.form.custom_kind == ConnectionKind::Mcp {
        return super::mcp_add::render_mcp_add(pane, live, theme, cx);
    }
    let llm_chips = build_llm_provider_chips(providers, live, &pane.hub, theme, cx);
    let custom_section = render_custom_connection_section(pane, live, theme, cx);
    let pane_entity = cx.entity();
    let custom_open = pane.form.custom_connection_open;

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .when(!llm_chips.is_empty(), |this| {
            this.child(render_chip_section("AIプロバイダを追加", llm_chips, theme))
        })
        .child(render_chip_section(
            "音声・埋め込み機能を追加",
            build_voice_embedding_chips(live, &pane.hub, theme, cx),
            theme,
        ))
        .child(render_chip_section(
            "Web 検索を追加",
            build_search_preset_chips(live, &pane.hub, theme, cx),
            theme,
        ))
        .child(disclosure_row(
            "custom-connection",
            "カスタム接続",
            custom_open,
            theme,
            move |_, cx| {
                pane_entity.update(cx, |this, cx| {
                    this.form.custom_connection_open = !this.form.custom_connection_open;
                    cx.notify();
                });
            },
        ))
        .when_some(custom_section, gpui::ParentElement::child)
        .into_any_element()
}

fn render_custom_connection_section(
    pane: &mut ConnectionsPane,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<gpui::Div> {
    if !pane.form.custom_connection_open {
        return None;
    }
    let custom_kind = pane.form.custom_kind;
    let kind_chips = build_custom_kind_chips(custom_kind, theme, cx);
    let (adapter_label, adapter_placeholder, target_label, target_placeholder) =
        custom_kind_placeholders(custom_kind);

    let name_d = pane.drafts.draft(
        cx,
        "add:custom:name",
        "表示名（例: ローカル LLM）",
        None,
        theme,
    );
    let adapter_d = pane
        .drafts
        .draft(cx, "add:custom:adapter", adapter_placeholder, None, theme);
    adapter_d.update(cx, |d, cx| d.set_placeholder(adapter_placeholder, cx));
    let target_d = pane
        .drafts
        .draft(cx, "add:custom:target", target_placeholder, None, theme);
    target_d.update(cx, |d, cx| d.set_placeholder(target_placeholder, cx));

    let c_name = name_d.read(cx).text().trim().to_owned();
    let c_adapter = adapter_d.read(cx).text().trim().to_owned();
    let c_target = target_d.read(cx).text().trim().to_owned();
    let hub_custom = pane.hub.clone();
    let pane_done = cx.entity();

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
                        pane_done.update(cx, |this, cx| {
                            this.form.adding_connection = false;
                            cx.notify();
                        });
                    },
                )),
            ),
    )
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
    cx: &Context<ConnectionsPane>,
) -> Vec<AnyElement> {
    providers
        .iter()
        .map(|prov| {
            let p = prov.clone();
            let hub = hub.clone();
            let pane = cx.entity();
            pill_button(
                SharedString::from(format!("add-llm-prov-{}", prov.id)),
                prov.name.clone(),
                Tone::Soft.icon(Icon::Plus).enabled(live),
                theme,
                move |_, cx| {
                    hub.update(cx, |hub, cx| {
                        hub.add_llm_from_provider(p.clone(), vec![], cx)
                    });
                    pane.update(cx, |this, cx| {
                        this.form.adding_connection = false;
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
