//! Companion template import and creation panels in `SettingsStudio`.

use super::{CompanionsPane, CompanionsSnapshot};
use crate::{
    input::Draft,
    ui::{
        NewCompanionSpec, SettingsSection,
        icons::Icon,
        studio::labeled_field,
        theme::Theme,
        widgets::{Tone, pill_button},
    },
};
use ene_client_core::domain::CompanionTemplate;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_import_panel(
    pane: &mut CompanionsPane,
    live: bool,
    templates: &[CompanionTemplate],
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let json_d = pane.drafts.description_draft(
        cx,
        "import:json",
        r#"{"format_version":1,"name":"...", "mind":"..."}"#,
        None,
        theme,
    );
    let json_text = json_d.read(cx).text().to_owned();
    let hub_import_json = pane.hub.clone();
    let pane_close = cx.entity();

    let starter_buttons: Vec<AnyElement> = templates
        .iter()
        .map(|template| {
            let tpl = template.clone();
            let hub_tpl = pane.hub.clone();
            pill_button(
                SharedString::from(format!("comp-starter-{}", tpl.id)),
                tpl.name.clone(),
                Tone::Soft.icon(Icon::Plus).enabled(live),
                theme,
                move |_, cx| {
                    hub_tpl.update(cx, |hub, cx| {
                        hub.import_companion_template(tpl.clone(), cx);
                    });
                },
            )
        })
        .collect();

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
        .when(!starter_buttons.is_empty(), |this| {
            this.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink_muted)
                            .child("スターターテンプレートから追加"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.))
                            .children(starter_buttons),
                    ),
            )
        })
        .child(labeled_field(
            "配布用テンプレート JSON を貼り付けて作成（記憶やキーは含まれません）",
            json_d,
            theme,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.))
                .child(pill_button(
                    "comp-import-cancel",
                    "閉じる",
                    Tone::Ghost,
                    theme,
                    move |_, cx| {
                        pane_close.update(cx, |this, cx| {
                            this.form.importing_template = false;
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    "comp-import-submit",
                    "JSON から作成",
                    Tone::Primary
                        .icon(Icon::Check)
                        .enabled(live && !json_text.trim().is_empty()),
                    theme,
                    move |_, cx| {
                        hub_import_json.update(cx, |hub, cx| {
                            hub.import_companion_json(json_text.clone(), cx);
                        });
                    },
                )),
        )
        .into_any_element()
}

pub(super) fn render_create_panel(
    pane: &mut CompanionsPane,
    snap: &CompanionsSnapshot,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let name_d = pane.drafts.draft(cx, "new:name", "例: ene", None, theme);
    let mind_d = pane.drafts.description_draft(
        cx,
        "new:mind",
        "例: 相手の気持ちに寄り添い、率直に対話する",
        None,
        theme,
    );
    let persona_d = pane.drafts.description_draft(
        cx,
        "new:persona",
        "例: 柔らかく丁寧で、親しみやすい口調",
        None,
        theme,
    );
    let name = name_d.read(cx).text().trim().to_owned();
    let mind = mind_d.read(cx).text().trim().to_owned();
    let persona = persona_d.read(cx).text().trim().to_owned();

    let template_chips =
        create_template_chips(&snap.templates, &name_d, &mind_d, &persona_d, theme);
    let no_models_banner =
        (!snap.has_models).then(|| render_missing_model_banner(pane.hub.clone(), theme));
    let actions = render_create_actions(
        pane,
        snap,
        NewCompanionSpec {
            name,
            persona,
            mind,
            voice_identifier: None,
        },
        theme,
        cx,
    );

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
        .when(!template_chips.is_empty(), |this| {
            this.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("雛形を反映:"),
                    )
                    .children(template_chips),
            )
        })
        .child(labeled_field("名前", name_d, theme))
        .child(labeled_field("人格の核", mind_d, theme))
        .child(labeled_field("話し方", persona_d, theme))
        .when_some(snap.preview_reply.as_ref().cloned(), |this, outcome| {
            this.child(render_preview_outcome(outcome, theme))
        })
        .when_some(no_models_banner, |this, banner| this.child(banner))
        .child(actions)
        .into_any_element()
}

fn render_create_actions(
    pane: &CompanionsPane,
    snap: &CompanionsSnapshot,
    spec: NewCompanionSpec,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let hub_create = pane.hub.clone();
    let hub_preview = pane.hub.clone();
    let pane_cancel = cx.entity();
    let pane_preview = cx.entity();
    let preview_pending = pane.form.preview_pending;
    let can_preview = snap.live
        && snap.has_models
        && !preview_pending
        && !spec.name.is_empty()
        && !spec.mind.is_empty();
    let can_create = snap.live && !spec.name.is_empty();
    let name_for_preview = spec.name.clone();
    let mind_for_preview = spec.mind.clone();
    let persona_for_preview = spec.persona.clone();

    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap(px(8.))
        .child(pill_button(
            "comp-create-cancel",
            "キャンセル",
            Tone::Ghost,
            theme,
            move |_, cx| {
                pane_cancel.update(cx, |this, cx| {
                    this.form.creating_companion = false;
                    this.form.preview_pending = false;
                    cx.notify();
                });
            },
        ))
        .child(pill_button(
            "comp-create-preview",
            if preview_pending {
                "試用中…"
            } else {
                "プレビュー"
            },
            Tone::Soft.icon(Icon::Sparkles).enabled(can_preview),
            theme,
            move |_, cx| {
                pane_preview.update(cx, |this, cx| {
                    this.form.preview_pending = true;
                    cx.notify();
                });
                hub_preview.update(cx, |hub, cx| {
                    hub.preview_companion(
                        name_for_preview.clone(),
                        mind_for_preview.clone(),
                        persona_for_preview.clone(),
                        cx,
                    );
                });
            },
        ))
        .child(pill_button(
            "comp-create-submit",
            "作成",
            Tone::Primary.icon(Icon::Check).enabled(can_create),
            theme,
            move |_, cx| {
                hub_create.update(cx, |hub, cx| {
                    hub.create_companion(
                        NewCompanionSpec {
                            name: spec.name.clone(),
                            persona: spec.persona.clone(),
                            mind: spec.mind.clone(),
                            voice_identifier: None,
                        },
                        cx,
                    );
                });
            },
        ))
        .into_any_element()
}

fn create_template_chips(
    templates: &[CompanionTemplate],
    name_d: &Entity<Draft>,
    mind_d: &Entity<Draft>,
    persona_d: &Entity<Draft>,
    theme: &Theme,
) -> Vec<AnyElement> {
    templates
        .iter()
        .map(|tpl| {
            let tid = tpl.id.clone();
            let tname = tpl.name.clone();
            let tmind = tpl.mind.clone();
            let tpersona = tpl.persona.clone().unwrap_or_default();
            let n_d = name_d.clone();
            let m_d = mind_d.clone();
            let p_d = persona_d.clone();
            pill_button(
                SharedString::from(format!("comp-new-tpl-{tid}")),
                tname.clone(),
                Tone::Ghost,
                theme,
                move |_, cx| {
                    n_d.update(cx, |d, cx| d.set(tname.clone(), cx));
                    m_d.update(cx, |d, cx| d.set(tmind.clone(), cx));
                    p_d.update(cx, |d, cx| d.set(tpersona.clone(), cx));
                },
            )
        })
        .collect()
}

fn render_preview_outcome(
    outcome: Result<String, ene_client_core::state::PreviewError>,
    theme: &Theme,
) -> AnyElement {
    let (bg, fg, text) = match outcome {
        Ok(reply) => (
            theme.bg_blush,
            theme.ink,
            format!("プレビュー応答: {reply}"),
        ),
        Err(err) => (
            theme.danger_soft,
            theme.danger_fg,
            format!("プレビュー失敗: {err}"),
        ),
    };
    div()
        .rounded(px(10.))
        .bg(bg)
        .px(px(12.))
        .py(px(8.))
        .text_xs()
        .text_color(fg)
        .child(text)
        .into_any_element()
}

fn render_missing_model_banner(hub: Entity<crate::ui::Hub>, theme: &Theme) -> AnyElement {
    div()
        .rounded(px(10.))
        .bg(theme.amber_soft)
        .px(px(12.))
        .py(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .text_color(theme.amber_fg)
                .child("有効なAIモデルが未設定です。接続からLLMを追加すると会話できます。"),
        )
        .child(pill_button(
            "comp-goto-conn",
            "接続へ",
            Tone::Soft.icon(Icon::Plug),
            theme,
            move |_, cx| {
                hub.update(cx, |hub, cx| {
                    hub.windows.settings_section = SettingsSection::Connections;
                    hub.load_providers(cx);
                    cx.notify();
                });
            },
        ))
        .into_any_element()
}
