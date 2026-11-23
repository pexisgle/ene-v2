//! The companion editor's visual sections.

//! Per-companion profile, VRM model, voice binding, and mind profile editor.

use super::super::{CompanionsPane, split_pipe};
use crate::{
    input::Draft,
    ui::{
        CompanionProfilePatch,
        icons::Icon,
        studio::labeled_field,
        theme::Theme,
        widgets::{DropdownSpec, Tone, disclosure_row, dropdown_select, pill_button},
    },
};
use ene_client_core::domain::MindProfile;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

use super::form::*;

pub(super) fn render_voice_selector(
    pane: &mut CompanionsPane,
    ctx: &EditorContext<'_>,
    drafts: &CompanionEditorDrafts,
    voice_selection: (&str, Option<&str>, Option<&str>),
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let id = &ctx.comp.id;
    let (voice_key_val, picked_conn_id, picked_voice_id) = voice_selection;
    let voice_dd_key = format!("comp-voice:{id}");
    let voice_dd_open = pane.open_dropdown.as_deref() == Some(voice_dd_key.as_str());
    let voice_filter_d = pane.drafts.draft(
        cx,
        format!("comp:{id}:voice-filter"),
        "話者名・IDで絞り込み…",
        None,
        theme,
    );
    let voice_filter_txt = voice_filter_d.read(cx).text().to_owned();
    let voice_options = build_voice_options(ctx.tts_voices);
    let selected_voice_label =
        format_selected_voice_label(ctx.tts_voices, picked_conn_id, picked_voice_id);
    let selected_dropdown_val = if !voice_key_val.is_empty() {
        Some(voice_key_val)
    } else {
        Some("")
    };

    let pane_voice_toggle = cx.entity();
    let pane_voice_pick = cx.entity();
    let toggle_key = voice_dd_key.clone();
    let voice_key_d_pick = drafts.voice_key_d.clone();
    let voice_d_pick = drafts.voice_d.clone();
    let voice_checking = {
        let hub = pane.hub.read(cx);
        ctx.tts_conn_ids
            .iter()
            .any(|cid| hub.setup.checking_connections.contains(cid))
    };

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(render_voice_header_row(
            id,
            voice_checking,
            ctx,
            &pane.hub,
            theme,
        ))
        .child(dropdown_select(
            DropdownSpec {
                id: voice_dd_key,
                selected_value: selected_dropdown_val,
                selected_label: selected_voice_label.as_deref(),
                placeholder: "接続の既定を使用",
                options: &voice_options,
                open: voice_dd_open,
                enabled: true,
                max_list_height: 210.,
                filter_draft: Some(voice_filter_d),
                filter_text: &voice_filter_txt,
                empty_label: "有効なTTS接続から話者を取得できませんでした",
            },
            theme,
            move |_, cx| {
                pane_voice_toggle.update(cx, |this, cx| {
                    this.open_dropdown =
                        if this.open_dropdown.as_deref() == Some(toggle_key.as_str()) {
                            None
                        } else {
                            Some(toggle_key.clone())
                        };
                    cx.notify();
                });
            },
            move |picked, _, cx| {
                let raw_vid = picked
                    .split_once('\0')
                    .map_or_else(|| picked.clone(), |(_, v)| v.to_owned());
                voice_key_d_pick.update(cx, |d, cx| d.set(picked, cx));
                voice_d_pick.update(cx, |d, cx| d.set(raw_vid, cx));
                pane_voice_pick.update(cx, |this, cx| {
                    this.open_dropdown = None;
                    cx.notify();
                });
            },
        ))
        .when(ctx.tts_voices.is_empty(), |this| {
            this.child(drafts.voice_d.clone())
        })
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(div().flex_1().child(labeled_field(
                    "話速",
                    drafts.voice_speed_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "音高",
                    drafts.voice_pitch_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "調子",
                    drafts.voice_style_d.clone(),
                    theme,
                ))),
        )
        .into_any_element()
}

pub(super) fn render_mind_section(
    pane: &mut CompanionsPane,
    ctx: &EditorContext<'_>,
    mind_val: &str,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> (AnyElement, Option<AnyElement>) {
    let id = &ctx.comp.id;
    let pane_disc = cx.entity();
    let mind_open = pane.form.mind_disclosure_open;
    let disclosure = disclosure_row(
        SharedString::from(format!("comp-mind-disc-{id}")),
        "詳細設定（性格の側面・セリフ例・背景・表現のクセ）",
        mind_open,
        theme,
        move |_, cx| {
            pane_disc.update(cx, |this, cx| {
                this.form.mind_disclosure_open = !this.form.mind_disclosure_open;
                cx.notify();
            });
        },
    );
    if !mind_open {
        return (disclosure, None);
    }

    let profile = ctx
        .mind_view
        .and_then(|v| v.profile.as_ref())
        .cloned()
        .unwrap_or_default();
    let prev = profile.revision;
    let facets_init = profile.facets.join(" | ");
    let examples_init = profile.examples.join(" | ");
    let bg_init = profile.background.join(" | ");
    let expr_init = profile.expression.join(" | ");

    let facets_d = pane.drafts.description_draft(
        cx,
        format!("mind:{id}:r{prev}:facets"),
        "「|」区切り",
        Some(&facets_init),
        theme,
    );
    let examples_d = pane.drafts.description_draft(
        cx,
        format!("mind:{id}:r{prev}:examples"),
        "「|」区切り",
        Some(&examples_init),
        theme,
    );
    let bg_d = pane.drafts.description_draft(
        cx,
        format!("mind:{id}:r{prev}:bg"),
        "「|」区切り",
        Some(&bg_init),
        theme,
    );
    let expr_d = pane.drafts.description_draft(
        cx,
        format!("mind:{id}:r{prev}:expr"),
        "「|」区切り",
        Some(&expr_init),
        theme,
    );

    let next_profile = MindProfile {
        core: if mind_val.is_empty() {
            profile.core
        } else {
            mind_val.to_owned()
        },
        revision: prev,
        facets: split_pipe(facets_d.read(cx).text()),
        examples: split_pipe(examples_d.read(cx).text()),
        background: split_pipe(bg_d.read(cx).text()),
        expression: split_pipe(expr_d.read(cx).text()),
    };
    let comp_id_save = id.to_owned();
    let hub_mind = pane.hub.clone();
    let strip = div()
        .rounded(px(10.))
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(labeled_field("性格の側面", facets_d, theme))
        .child(labeled_field("セリフ例", examples_d, theme))
        .child(labeled_field("背景・設定", bg_d, theme))
        .child(labeled_field("表現のクセ", expr_d, theme))
        .child(div().flex().flex_row().justify_end().child(pill_button(
            SharedString::from(format!("comp-save-mind-{id}")),
            "詳細を保存",
            Tone::Soft.icon(Icon::Check).enabled(ctx.live),
            theme,
            move |_, cx| {
                hub_mind.update(cx, |hub, cx| {
                    hub.update_mind_profile(comp_id_save.clone(), prev, next_profile.clone(), cx);
                });
            },
        )))
        .into_any_element();

    (disclosure, Some(strip))
}

pub(super) fn render_companion_footer(
    pane: &CompanionsPane,
    id: &str,
    live: bool,
    patch: CompanionProfilePatch,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let armed = pane.form.delete_armed_companion.as_deref() == Some(id);
    let pane_arm = cx.entity();
    let pane_disarm = cx.entity();
    let hub_del = pane.hub.clone();
    let hub_save = pane.hub.clone();
    let hub_export = pane.hub.clone();
    let arm_id = id.to_owned();
    let del_id = id.to_owned();
    let export_id = id.to_owned();
    let can_save = live && !patch.name.is_empty();

    let delete_controls = if armed {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child("本当に削除しますか？"),
            )
            .child(pill_button(
                SharedString::from(format!("comp-del-cancel-{id}")),
                "キャンセル",
                Tone::Ghost,
                theme,
                move |_, cx| {
                    pane_disarm.update(cx, |this, cx| {
                        this.form.delete_armed_companion = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                SharedString::from(format!("comp-del-exec-{id}")),
                "削除を実行",
                Tone::Danger.icon(Icon::Trash).enabled(live),
                theme,
                move |_, cx| {
                    hub_del.update(cx, |hub, cx| hub.delete_companion(del_id.clone(), cx));
                },
            ))
    } else {
        div().child(pill_button(
            SharedString::from(format!("comp-del-arm-{id}")),
            "削除",
            Tone::Danger.icon(Icon::Trash).enabled(live),
            theme,
            move |_, cx| {
                pane_arm.update(cx, |this, cx| {
                    this.form.delete_armed_companion = Some(arm_id.clone());
                    cx.notify();
                });
            },
        ))
    };

    div()
        .pt(px(4.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(10.))
        .child(delete_controls)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(pill_button(
                    SharedString::from(format!("comp-export-{id}")),
                    "テンプレート出力",
                    Tone::Ghost.enabled(live),
                    theme,
                    move |_, cx| {
                        hub_export.update(cx, |hub, cx| {
                            hub.export_companion(export_id.clone(), cx);
                        });
                    },
                ))
                .child(pill_button(
                    SharedString::from(format!("comp-save-{id}")),
                    "保存",
                    Tone::Primary.icon(Icon::Check).enabled(can_save),
                    theme,
                    move |_, cx| {
                        hub_save.update(cx, |hub, cx| {
                            hub.update_companion_profile(
                                CompanionProfilePatch {
                                    companion_id: patch.companion_id.clone(),
                                    name: patch.name.clone(),
                                    persona: patch.persona.clone(),
                                    mind: patch.mind.clone(),
                                    body: patch.body.clone(),
                                    voice: patch.voice.clone(),
                                },
                                cx,
                            );
                        });
                    },
                )),
        )
        .into_any_element()
}

pub(super) fn render_body_model_field(
    id: &str,
    body_model_d: Entity<Draft>,
    theme: &Theme,
) -> AnyElement {
    let model_d_pick = body_model_d.clone();
    let model_d_default = body_model_d.clone();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink_muted)
                        .child("見た目（VRM モデル）"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(pill_button(
                            SharedString::from(format!("comp-model-default-{id}")),
                            "ENE.vrm",
                            Tone::Ghost,
                            theme,
                            move |_, cx| {
                                model_d_default.update(cx, |d, cx| d.set("ENE.vrm".to_owned(), cx));
                            },
                        ))
                        .child(pill_button(
                            SharedString::from(format!("comp-model-browse-{id}")),
                            "ファイルを選択",
                            Tone::Soft,
                            theme,
                            move |_, cx| {
                                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                    files: true,
                                    directories: false,
                                    multiple: false,
                                    prompt: Some("VRM モデル (.vrm) を選択".into()),
                                });
                                let draft = model_d_pick.clone();
                                cx.spawn(async move |cx| {
                                    if let Ok(Ok(Some(paths))) = rx.await
                                        && let Some(path) = paths.into_iter().next()
                                    {
                                        let path_str = path.to_string_lossy().into_owned();
                                        let _ = cx.update(|cx| {
                                            draft.update(cx, |d, cx| d.set(path_str, cx));
                                        });
                                    }
                                })
                                .detach();
                            },
                        )),
                ),
        )
        .child(body_model_d)
        .into_any_element()
}

pub(super) fn render_exported_template_box(id: &str, json: &str, theme: &Theme) -> AnyElement {
    div()
        .rounded(px(10.))
        .bg(theme.bg_blush)
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child("書き出したテンプレート JSON（秘密情報・記憶なし）"),
        )
        .child(
            div()
                .id(SharedString::from(format!("comp-export-json-{id}")))
                .max_h(px(140.))
                .overflow_y_scroll()
                .text_xs()
                .text_color(theme.ink)
                .child(json.to_owned()),
        )
        .into_any_element()
}

pub(super) fn render_voice_header_row(
    id: &str,
    voice_checking: bool,
    ctx: &EditorContext<'_>,
    hub: &Entity<crate::ui::Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_refresh_voice = hub.clone();
    let refresh_tts_ids = ctx.tts_conn_ids.to_vec();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child("声（接続・話者）"),
        )
        .when(!refresh_tts_ids.is_empty(), |this| {
            this.child(pill_button(
                SharedString::from(format!("comp-voice-refresh-{id}")),
                if voice_checking {
                    "話者を取得中…"
                } else {
                    "話者一覧を取得"
                },
                Tone::Ghost
                    .icon(Icon::Refresh)
                    .enabled(ctx.live && !voice_checking),
                theme,
                move |_, cx| {
                    hub_refresh_voice.update(cx, |hub, cx| {
                        for cid in &refresh_tts_ids {
                            hub.check_connection(cid.clone(), cx);
                        }
                    });
                },
            ))
        })
}
