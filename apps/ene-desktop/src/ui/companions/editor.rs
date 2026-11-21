//! Per-companion profile, VRM model, voice binding, and mind profile editor.

use super::{parse_voice_selection, split_pipe};
use crate::{
    input::Draft,
    ui::{
        CompanionProfilePatch, SettingsStudio,
        icons::Icon,
        studio::labeled_field,
        theme::Theme,
        widgets::{
            DropdownOption, DropdownSpec, Tone, disclosure_row, dropdown_select, pill_button,
        },
    },
};
use ene_rpc as w;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

pub(super) struct EditorContext<'a> {
    pub(super) comp: &'a w::Companion,
    pub(super) mind_view: Option<&'a w::MindView>,
    pub(super) tts_voices: &'a [(String, String, String, String)],
    pub(super) tts_conn_ids: &'a [String],
    pub(super) exported_json: Option<&'a str>,
    pub(super) live: bool,
}

struct CompanionEditorDrafts {
    name_d: Entity<Draft>,
    mind_d: Entity<Draft>,
    persona_d: Entity<Draft>,
    body_model_d: Entity<Draft>,
    voice_key_d: Entity<Draft>,
    voice_d: Entity<Draft>,
    voice_speed_d: Entity<Draft>,
    voice_pitch_d: Entity<Draft>,
    voice_style_d: Entity<Draft>,
}

impl SettingsStudio {
    pub(super) fn render_companion_editor(
        &mut self,
        ctx: EditorContext<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ctx.comp.id.clone();
        let current_body = ctx
            .comp
            .body
            .as_option()
            .and_then(|doc| serde_json::from_str::<w::BodyBinding>(&doc.json).ok())
            .unwrap_or_default();
        let drafts = self.init_companion_editor_drafts(ctx.comp, &current_body, theme, cx);

        let name_val = drafts.name_d.read(cx).text().trim().to_owned();
        let mind_val = drafts.mind_d.read(cx).text().trim().to_owned();
        let persona_val = drafts.persona_d.read(cx).text().trim().to_owned();
        let body_model_val = drafts.body_model_d.read(cx).text().trim().to_owned();
        let voice_key_val = drafts.voice_key_d.read(cx).text().to_owned();
        let voice_manual_val = drafts.voice_d.read(cx).text().trim().to_owned();
        let (picked_conn_id, picked_voice_id) =
            parse_voice_selection(&voice_key_val, &voice_manual_val);
        let mut next_body = current_body;
        next_body.model = (!body_model_val.is_empty()).then_some(body_model_val);
        let next_voice = w::VoiceBinding {
            connection_id: picked_conn_id.clone(),
            voice_id: picked_voice_id.clone(),
            speed: parse_finite_f32(&drafts.voice_speed_d, cx),
            pitch: parse_finite_f32(&drafts.voice_pitch_d, cx),
            style: non_empty_trimmed(&drafts.voice_style_d, cx),
        };

        let model_field = render_body_model_field(&id, drafts.body_model_d.clone(), theme);
        let voice_selector = self.render_voice_selector(
            &ctx,
            &drafts,
            (
                &voice_key_val,
                picked_conn_id.as_deref(),
                picked_voice_id.as_deref(),
            ),
            theme,
            cx,
        );
        let (disclosure, mind_strip) = self.render_mind_section(&ctx, &mind_val, theme, cx);
        let footer = self.render_companion_footer(
            &id,
            ctx.live,
            CompanionProfilePatch {
                companion_id: id.clone(),
                name: name_val,
                persona: persona_val,
                mind: mind_val,
                body: next_body,
                voice: next_voice,
            },
            theme,
            cx,
        );

        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(labeled_field("名前", drafts.name_d, theme))
            .child(labeled_field("人格の核", drafts.mind_d, theme))
            .child(labeled_field("話し方", drafts.persona_d, theme))
            .child(model_field)
            .child(voice_selector)
            .child(disclosure)
            .when_some(mind_strip, |this, strip| this.child(strip))
            .when_some(ctx.exported_json, |this, json| {
                this.child(render_exported_template_box(&id, json, theme))
            })
            .child(footer)
            .into_any_element()
    }

    fn init_companion_editor_drafts(
        &mut self,
        comp: &w::Companion,
        current_body: &w::BodyBinding,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> CompanionEditorDrafts {
        let id = &comp.id;
        let rev = comp.revision;
        let mut current_voice = comp
            .voice
            .as_option()
            .and_then(|doc| serde_json::from_str::<w::VoiceBinding>(&doc.json).ok())
            .unwrap_or_default();
        if current_voice
            .voice_id
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
        {
            current_voice.voice_id = comp.voice_identifier.clone();
        }
        let initial_voice_key = match (
            current_voice
                .connection_id
                .as_deref()
                .filter(|s| !s.is_empty()),
            current_voice.voice_id.as_deref().filter(|s| !s.is_empty()),
        ) {
            (Some(cid), Some(vid)) => format!("{cid}\0{vid}"),
            (None, Some(vid)) => format!("\0{vid}"),
            _ => String::new(),
        };
        let initial_speed = current_voice.speed.map(|v| format!("{v:.2}"));
        let initial_pitch = current_voice.pitch.map(|v| format!("{v:.2}"));

        CompanionEditorDrafts {
            name_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:name"),
                "名前",
                Some(&comp.name),
                theme,
            ),
            mind_d: self.description_draft(
                cx,
                format!("comp:{id}:r{rev}:mind"),
                "人格の核",
                Some(&comp.mind),
                theme,
            ),
            persona_d: self.description_draft(
                cx,
                format!("comp:{id}:r{rev}:persona"),
                "話し方",
                comp.persona.as_deref(),
                theme,
            ),
            body_model_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:body-model"),
                "VRM モデル（例: ENE.vrm / Seed-san.vrm / .vrm パス）",
                current_body.model.as_deref(),
                theme,
            ),
            voice_key_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:voice-key"),
                "",
                Some(&initial_voice_key),
                theme,
            ),
            voice_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:voice"),
                "話者ID（任意）",
                current_voice.voice_id.as_deref(),
                theme,
            ),
            voice_speed_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:voice-speed"),
                "速度（例: 1.0）",
                initial_speed.as_deref(),
                theme,
            ),
            voice_pitch_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:voice-pitch"),
                "音高（例: 0.0）",
                initial_pitch.as_deref(),
                theme,
            ),
            voice_style_d: self.draft(
                cx,
                format!("comp:{id}:r{rev}:voice-style"),
                "例: 丁寧・元気（任意）",
                current_voice.style.as_deref(),
                theme,
            ),
        }
    }

    fn render_voice_selector(
        &mut self,
        ctx: &EditorContext<'_>,
        drafts: &CompanionEditorDrafts,
        voice_selection: (&str, Option<&str>, Option<&str>),
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = &ctx.comp.id;
        let (voice_key_val, picked_conn_id, picked_voice_id) = voice_selection;
        let voice_dd_key = format!("comp-voice:{id}");
        let voice_dd_open = self.open_dropdown.as_deref() == Some(voice_dd_key.as_str());
        let voice_filter_d = self.draft(
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

        let studio_voice_toggle = cx.entity();
        let studio_voice_pick = cx.entity();
        let toggle_key = voice_dd_key.clone();
        let voice_key_d_pick = drafts.voice_key_d.clone();
        let voice_d_pick = drafts.voice_d.clone();
        let voice_checking = {
            let hub = self.hub.read(cx);
            ctx.tts_conn_ids
                .iter()
                .any(|cid| hub.checking_connections.contains(cid))
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
                &self.hub,
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
                    studio_voice_toggle.update(cx, |this, cx| {
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
                    studio_voice_pick.update(cx, |this, cx| {
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

    fn render_mind_section(
        &mut self,
        ctx: &EditorContext<'_>,
        mind_val: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let id = &ctx.comp.id;
        let studio_disc = cx.entity();
        let mind_open = self.companion_form.mind_disclosure_open;
        let disclosure = disclosure_row(
            SharedString::from(format!("comp-mind-disc-{id}")),
            "詳細設定（性格の側面・セリフ例・背景・表現のクセ）",
            mind_open,
            theme,
            move |_, cx| {
                studio_disc.update(cx, |this, cx| {
                    this.companion_form.mind_disclosure_open =
                        !this.companion_form.mind_disclosure_open;
                    cx.notify();
                });
            },
        );
        if !mind_open {
            return (disclosure, None);
        }

        let profile = ctx
            .mind_view
            .and_then(|v| v.profile.as_option())
            .cloned()
            .unwrap_or_default();
        let prev = profile.revision;
        let facets_init = profile.facets.join(" | ");
        let examples_init = profile.examples.join(" | ");
        let bg_init = profile.background.join(" | ");
        let expr_init = profile.expression.join(" | ");

        let facets_d = self.description_draft(
            cx,
            format!("mind:{id}:r{prev}:facets"),
            "「|」区切り",
            Some(&facets_init),
            theme,
        );
        let examples_d = self.description_draft(
            cx,
            format!("mind:{id}:r{prev}:examples"),
            "「|」区切り",
            Some(&examples_init),
            theme,
        );
        let bg_d = self.description_draft(
            cx,
            format!("mind:{id}:r{prev}:bg"),
            "「|」区切り",
            Some(&bg_init),
            theme,
        );
        let expr_d = self.description_draft(
            cx,
            format!("mind:{id}:r{prev}:expr"),
            "「|」区切り",
            Some(&expr_init),
            theme,
        );

        let next_profile = w::MindProfile {
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
            ..Default::default()
        };
        let comp_id_save = id.to_owned();
        let hub_mind = self.hub.clone();
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
                        hub.update_mind_profile(
                            comp_id_save.clone(),
                            prev,
                            next_profile.clone(),
                            cx,
                        );
                    });
                },
            )))
            .into_any_element();

        (disclosure, Some(strip))
    }

    fn render_companion_footer(
        &self,
        id: &str,
        live: bool,
        patch: CompanionProfilePatch,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed = self.companion_form.delete_armed_companion.as_deref() == Some(id);
        let studio_arm = cx.entity();
        let studio_disarm = cx.entity();
        let hub_del = self.hub.clone();
        let hub_save = self.hub.clone();
        let hub_export = self.hub.clone();
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
                        studio_disarm.update(cx, |this, cx| {
                            this.companion_form.delete_armed_companion = None;
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
                    studio_arm.update(cx, |this, cx| {
                        this.companion_form.delete_armed_companion = Some(arm_id.clone());
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
}

fn render_body_model_field(id: &str, body_model_d: Entity<Draft>, theme: &Theme) -> AnyElement {
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

fn render_exported_template_box(id: &str, json: &str, theme: &Theme) -> AnyElement {
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

fn build_voice_options(tts_voices: &[(String, String, String, String)]) -> Vec<DropdownOption> {
    let mut voice_options = vec![DropdownOption {
        value: String::new(),
        label: "接続の既定を使用".to_owned(),
        detail: None,
    }];
    for (cid, vid, label, conn_name) in tts_voices {
        voice_options.push(DropdownOption {
            value: format!("{cid}\0{vid}"),
            label: label.clone(),
            detail: Some(format!("{conn_name} / ID: {vid}")),
        });
    }
    voice_options
}

fn format_selected_voice_label(
    tts_voices: &[(String, String, String, String)],
    picked_conn_id: Option<&str>,
    picked_voice_id: Option<&str>,
) -> Option<String> {
    match (picked_conn_id, picked_voice_id) {
        (None, None) => Some("接続の既定を使用".to_owned()),
        (Some(cid), Some(vid)) => tts_voices
            .iter()
            .find(|(c, v, _, _)| c == cid && v == vid)
            .map(|(_, v, label, cname)| format!("{label} ({cname} / ID: {v})"))
            .or_else(|| Some(format!("{cid} / {vid}"))),
        (None, Some(vid)) => tts_voices
            .iter()
            .find(|(_, v, _, _)| v == vid)
            .map(|(_, v, label, cname)| format!("{label} ({cname} / ID: {v})"))
            .or_else(|| Some(vid.to_owned())),
        (Some(cid), None) => Some(cid.to_owned()),
    }
}

fn parse_finite_f32(draft: &Entity<Draft>, cx: &gpui::App) -> Option<f32> {
    draft
        .read(cx)
        .text()
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
}

fn non_empty_trimmed(draft: &Entity<Draft>, cx: &gpui::App) -> Option<String> {
    let s = draft.read(cx).text().trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn render_voice_header_row(
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
