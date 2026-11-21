//! Companions chapter in `SettingsStudio`: 3-field quick creation, 62px bordered rows,
//! 1-level hairline-divided editor, and collapsed `MindProfile` disclosure.

use super::SettingsStudio;
use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{
    DropdownOption, Tone, companion_badge, disclosure_row, dropdown_select, pill_button,
    status_pill,
};
use super::{NewCompanionSpec, SettingsSection};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

impl SettingsStudio {
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn render_companions(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (
            live,
            companions,
            active_id,
            resident_ids,
            has_models,
            tts_voices,
            tts_conn_ids,
            minds,
            templates,
            exported_template,
            preview_reply,
        ) = {
            let hub = self.hub.read(cx);
            let tts_ids: Vec<String> = hub
                .model
                .settings()
                .map(|s| {
                    s.connections
                        .iter()
                        .filter(|c| c.kind == w::ConnectionKind::Tts && c.enabled)
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            (
                hub.live(),
                hub.model
                    .snapshot()
                    .map(|s| s.companions.clone())
                    .unwrap_or_default(),
                hub.model.companion_id().map(str::to_owned),
                hub.model.resident_companions(&hub.options.id),
                !hub.model_choices().is_empty(),
                hub.tts_voice_choices(),
                tts_ids,
                hub.model.minds.clone(),
                hub.model.templates.clone(),
                hub.model.exported_template.clone(),
                hub.model.preview_reply.clone(),
            )
        };

        let studio_toggle_create = cx.entity();
        let studio_toggle_import = cx.entity();
        let creating = self.creating_companion;
        let importing = self.importing_template;
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(22.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("コンパニオン"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(pill_button(
                        "comp-toggle-import",
                        if importing {
                            "読込を閉じる"
                        } else {
                            "テンプレート読込"
                        },
                        None,
                        Tone::Ghost,
                        live,
                        theme,
                        move |_, cx| {
                            studio_toggle_import.update(cx, |this, cx| {
                                this.importing_template = !this.importing_template;
                                if this.importing_template {
                                    this.creating_companion = false;
                                }
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        "comp-toggle-create",
                        if creating {
                            "閉じる"
                        } else {
                            "新規作成"
                        },
                        (!creating).then_some(Icon::Plus),
                        if creating { Tone::Ghost } else { Tone::Primary },
                        true,
                        theme,
                        move |_, cx| {
                            studio_toggle_create.update(cx, |this, cx| {
                                this.creating_companion = !this.creating_companion;
                                if this.creating_companion {
                                    this.importing_template = false;
                                }
                                cx.notify();
                            });
                        },
                    )),
            );

        let import_panel = if importing {
            Some(self.render_import_panel(live, &templates, theme, cx))
        } else {
            None
        };
        let create_panel = if creating {
            Some(self.render_create_panel(
                live,
                has_models,
                &templates,
                preview_reply.as_ref(),
                theme,
                cx,
            ))
        } else {
            None
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        for comp in companions {
            let id = comp.id.clone();
            let rev = comp.revision;
            let active = active_id.as_deref() == Some(id.as_str());
            let resident = resident_ids.iter().any(|rid| rid == &id);
            let expanded = self.expanded_companion_id.as_deref() == Some(id.as_str());
            let persona_line = comp
                .persona
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("話し方未設定")
                .to_owned();

            let hub_select = self.hub.clone();
            let select_id = id.clone();
            let hub_res = self.hub.clone();
            let res_id = id.clone();
            let studio_expand = cx.entity();
            let expand_id = id.clone();
            let hub_expand = self.hub.clone();

            let row_header = div()
                .h(px(62.))
                .px(px(16.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.))
                        .child(companion_badge(
                            &comp.name,
                            &id,
                            34.,
                            (active || resident).then_some(theme.mint),
                            theme,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme.ink)
                                        .truncate()
                                        .child(comp.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.ink_muted)
                                        .truncate()
                                        .child(persona_line),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(pill_button(
                            SharedString::from(format!("comp-res-{id}")),
                            if resident { "常駐中" } else { "常駐" },
                            None,
                            if resident { Tone::Mint } else { Tone::Ghost },
                            live,
                            theme,
                            move |_, cx| {
                                hub_res.update(cx, |hub, cx| {
                                    if resident {
                                        hub.release_companion(res_id.clone(), cx);
                                    } else {
                                        hub.activate_companion(res_id.clone(), cx);
                                    }
                                });
                            },
                        ))
                        .child(if active {
                            status_pill("会話中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
                        } else {
                            pill_button(
                                SharedString::from(format!("comp-select-{id}")),
                                "選択",
                                None,
                                Tone::Soft,
                                live,
                                theme,
                                move |_, cx| {
                                    hub_select.update(cx, |hub, cx| {
                                        hub.select_companion(&select_id, cx);
                                    });
                                },
                            )
                        })
                        .child(pill_button(
                            SharedString::from(format!("comp-edit-{id}")),
                            if expanded { "閉じる" } else { "編集" },
                            None,
                            if expanded { Tone::Soft } else { Tone::Ghost },
                            true,
                            theme,
                            move |_, cx| {
                                let mut next_open = None;
                                studio_expand.update(cx, |this, cx| {
                                    if this.expanded_companion_id.as_deref()
                                        == Some(expand_id.as_str())
                                    {
                                        this.expanded_companion_id = None;
                                    } else {
                                        this.expanded_companion_id = Some(expand_id.clone());
                                        next_open = Some(expand_id.clone());
                                    }
                                    this.delete_armed_companion = None;
                                    cx.notify();
                                });
                                if let Some(target_id) = next_open {
                                    hub_expand.update(cx, |hub, _| {
                                        hub.load_mind_for(target_id);
                                    });
                                }
                            },
                        )),
                );

            let exported_for_id = exported_template
                .as_ref()
                .filter(|(cid, _)| cid == &id)
                .map(|(_, json)| json.as_str());
            let editor = if expanded {
                Some(self.render_companion_editor(
                    &comp,
                    rev,
                    minds.get(&id),
                    &tts_voices,
                    &tts_conn_ids,
                    exported_for_id,
                    live,
                    theme,
                    cx,
                ))
            } else {
                None
            };

            rows.push(
                div()
                    .w_full()
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(14.))
                    .flex()
                    .flex_col()
                    .child(row_header)
                    .when_some(editor, |this, ed| this.child(ed))
                    .into_any_element(),
            );
        }

        if rows.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(14.))
                    .px(px(16.))
                    .py(px(18.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("登録されているコンパニオンはありません。"),
                    )
                    .when(!creating, |this| {
                        let studio = cx.entity();
                        this.child(pill_button(
                            "comp-empty-create",
                            "新規作成",
                            Some(Icon::Plus),
                            Tone::Primary,
                            true,
                            theme,
                            move |_, cx| {
                                studio.update(cx, |this, cx| {
                                    this.creating_companion = true;
                                    cx.notify();
                                });
                            },
                        ))
                    })
                    .into_any_element(),
            );
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(import_panel, |this, p| this.child(p))
            .when_some(create_panel, |this, p| this.child(p))
            .child(div().w_full().flex().flex_col().gap(px(10.)).children(rows))
            .into_any_element()
    }

    fn render_import_panel(
        &mut self,
        live: bool,
        templates: &[w::CompanionTemplate],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let json_d = self.description_draft(
            cx,
            "import:json",
            r#"{"format_version":1,"name":"...", "mind":"..."}"#,
            None,
            theme,
        );
        let json_text = json_d.read(cx).text().to_owned();
        let hub_import_json = self.hub.clone();
        let studio_close = cx.entity();

        let mut starter_buttons: Vec<AnyElement> = Vec::new();
        for template in templates {
            let tpl = template.clone();
            let hub_tpl = self.hub.clone();
            let tid = tpl.id.clone();
            let label = tpl.name.clone();
            starter_buttons.push(pill_button(
                SharedString::from(format!("comp-starter-{tid}")),
                label,
                Some(Icon::Plus),
                Tone::Soft,
                live,
                theme,
                move |_, cx| {
                    hub_tpl.update(cx, |hub, cx| {
                        hub.import_companion_template(tpl.clone(), cx);
                    });
                },
            ));
        }

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
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_close.update(cx, |this, cx| {
                                this.importing_template = false;
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        "comp-import-submit",
                        "JSON から作成",
                        Some(Icon::Check),
                        Tone::Primary,
                        live && !json_text.trim().is_empty(),
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

    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn render_create_panel(
        &mut self,
        live: bool,
        has_models: bool,
        templates: &[w::CompanionTemplate],
        preview_reply: Option<&Result<String, String>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name_d = self.draft(cx, "new:name", "例: ene", None, theme);
        let mind_d = self.description_draft(
            cx,
            "new:mind",
            "例: 相手の気持ちに寄り添い、率直に対話する",
            None,
            theme,
        );
        let persona_d = self.description_draft(
            cx,
            "new:persona",
            "例: 柔らかく丁寧で、親しみやすい口調",
            None,
            theme,
        );

        let name = name_d.read(cx).text().trim().to_owned();
        let mind = mind_d.read(cx).text().trim().to_owned();
        let persona = persona_d.read(cx).text().trim().to_owned();

        let hub_to_conn = self.hub.clone();
        let hub_create = self.hub.clone();
        let hub_preview = self.hub.clone();
        let studio_cancel = cx.entity();
        let studio_preview = cx.entity();
        let preview_pending = self.preview_pending;

        let mut template_chips: Vec<AnyElement> = Vec::new();
        for tpl in templates {
            let tid = tpl.id.clone();
            let tname = tpl.name.clone();
            let tmind = tpl.mind.clone();
            let tpersona = tpl.persona.clone().unwrap_or_default();
            let n_d = name_d.clone();
            let m_d = mind_d.clone();
            let p_d = persona_d.clone();
            template_chips.push(pill_button(
                SharedString::from(format!("comp-new-tpl-{tid}")),
                tname.clone(),
                None,
                Tone::Ghost,
                true,
                theme,
                move |_, cx| {
                    n_d.update(cx, |d, cx| d.set(tname.clone(), cx));
                    m_d.update(cx, |d, cx| d.set(tmind.clone(), cx));
                    p_d.update(cx, |d, cx| d.set(tpersona.clone(), cx));
                },
            ));
        }

        let name_for_preview = name.clone();
        let mind_for_preview = mind.clone();
        let persona_for_preview = persona.clone();

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
            .when_some(preview_reply.cloned(), |this, outcome| {
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
                this.child(
                    div()
                        .rounded(px(10.))
                        .bg(bg)
                        .px(px(12.))
                        .py(px(8.))
                        .text_xs()
                        .text_color(fg)
                        .child(text),
                )
            })
            .when(!has_models, |this| {
                this.child(
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
                        .child(div().text_xs().text_color(theme.amber_fg).child(
                            "有効なAIモデルが未設定です。接続からLLMを追加すると会話できます。",
                        ))
                        .child(pill_button(
                            "comp-goto-conn",
                            "接続へ",
                            Some(Icon::Plug),
                            Tone::Soft,
                            true,
                            theme,
                            move |_, cx| {
                                hub_to_conn.update(cx, |hub, cx| {
                                    hub.settings_section = SettingsSection::Connections;
                                    hub.load_providers(cx);
                                    cx.notify();
                                });
                            },
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(pill_button(
                        "comp-create-cancel",
                        "キャンセル",
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_cancel.update(cx, |this, cx| {
                                this.creating_companion = false;
                                this.preview_pending = false;
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
                        Some(Icon::Sparkles),
                        Tone::Soft,
                        live && has_models
                            && !preview_pending
                            && !name.is_empty()
                            && !mind.is_empty(),
                        theme,
                        move |_, cx| {
                            studio_preview.update(cx, |this, cx| {
                                this.preview_pending = true;
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
                        Some(Icon::Check),
                        Tone::Primary,
                        live && !name.is_empty(),
                        theme,
                        move |_, cx| {
                            hub_create.update(cx, |hub, cx| {
                                hub.create_companion(
                                    NewCompanionSpec {
                                        name: name.clone(),
                                        persona: persona.clone(),
                                        mind: mind.clone(),
                                        voice_identifier: None,
                                    },
                                    cx,
                                );
                            });
                        },
                    )),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    fn render_companion_editor(
        &mut self,
        comp: &w::Companion,
        rev: u64,
        mind_view: Option<&w::MindView>,
        tts_voices: &[(String, String, String, String)],
        tts_conn_ids: &[String],
        exported_json: Option<&str>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = comp.id.clone();
        let current_body = comp
            .body
            .as_option()
            .and_then(|doc| serde_json::from_str::<w::BodyBinding>(&doc.json).ok())
            .unwrap_or_default();
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

        let name_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:name"),
            "名前",
            Some(&comp.name),
            theme,
        );
        let mind_d = self.description_draft(
            cx,
            format!("comp:{id}:r{rev}:mind"),
            "人格の核",
            Some(&comp.mind),
            theme,
        );
        let persona_d = self.description_draft(
            cx,
            format!("comp:{id}:r{rev}:persona"),
            "話し方",
            comp.persona.as_deref(),
            theme,
        );
        let body_model_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:body-model"),
            "VRM モデル（例: ENE.vrm / Seed-san.vrm / .vrm パス）",
            current_body.model.as_deref(),
            theme,
        );
        let voice_key_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-key"),
            "",
            Some(&initial_voice_key),
            theme,
        );
        let voice_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice"),
            "話者ID（任意）",
            current_voice.voice_id.as_deref(),
            theme,
        );
        let voice_speed_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-speed"),
            "速度（例: 1.0）",
            initial_speed.as_deref(),
            theme,
        );
        let voice_pitch_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-pitch"),
            "音高（例: 0.0）",
            initial_pitch.as_deref(),
            theme,
        );
        let voice_style_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-style"),
            "例: 丁寧・元気（任意）",
            current_voice.style.as_deref(),
            theme,
        );

        let name_val = name_d.read(cx).text().trim().to_owned();
        let mind_val = mind_d.read(cx).text().trim().to_owned();
        let persona_val = persona_d.read(cx).text().trim().to_owned();
        let body_model_val = body_model_d.read(cx).text().trim().to_owned();
        let voice_key_val = voice_key_d.read(cx).text().to_owned();
        let voice_manual_val = voice_d.read(cx).text().trim().to_owned();
        let (picked_conn_id, picked_voice_id) =
            parse_voice_selection(&voice_key_val, &voice_manual_val);
        let speed_val = voice_speed_d
            .read(cx)
            .text()
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite());
        let pitch_val = voice_pitch_d
            .read(cx)
            .text()
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite());
        let style_val = {
            let s = voice_style_d.read(cx).text().trim().to_owned();
            (!s.is_empty()).then_some(s)
        };
        let mut next_body = current_body.clone();
        next_body.model = (!body_model_val.is_empty()).then_some(body_model_val);
        let next_voice = w::VoiceBinding {
            connection_id: picked_conn_id.clone(),
            voice_id: picked_voice_id.clone(),
            speed: speed_val,
            pitch: pitch_val,
            style: style_val,
        };

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
        let selected_voice_label = match (picked_conn_id.as_deref(), picked_voice_id.as_deref()) {
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
        };
        let selected_dropdown_val = if !voice_key_val.is_empty() {
            Some(voice_key_val.as_str())
        } else {
            Some("")
        };
        let studio_voice_toggle = cx.entity();
        let studio_voice_pick = cx.entity();
        let toggle_key = voice_dd_key.clone();
        let voice_key_d_pick = voice_key_d.clone();
        let voice_d_pick = voice_d.clone();
        let hub_refresh_voice = self.hub.clone();
        let refresh_tts_ids = tts_conn_ids.to_vec();
        let voice_checking = {
            let hub = self.hub.read(cx);
            tts_conn_ids
                .iter()
                .any(|cid| hub.checking_connections.contains(cid))
        };
        let voice_selector = div()
            .w_full()
            .min_w_0()
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
                            Some(Icon::Refresh),
                            Tone::Ghost,
                            live && !voice_checking,
                            theme,
                            move |_, cx| {
                                hub_refresh_voice.update(cx, |hub, cx| {
                                    for cid in &refresh_tts_ids {
                                        hub.check_connection(cid.clone(), cx);
                                    }
                                });
                            },
                        ))
                    }),
            )
            .child(dropdown_select(
                voice_dd_key,
                selected_dropdown_val,
                selected_voice_label.as_deref(),
                "接続の既定を使用",
                &voice_options,
                voice_dd_open,
                true,
                210.,
                Some(voice_filter_d),
                &voice_filter_txt,
                "有効なTTS接続から話者を取得できませんでした",
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
            .when(tts_voices.is_empty(), |this| this.child(voice_d))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .child(labeled_field("話速", voice_speed_d, theme)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(labeled_field("音高", voice_pitch_d, theme)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(labeled_field("調子", voice_style_d, theme)),
                    ),
            );

        let studio_disc = cx.entity();
        let mind_open = self.mind_disclosure_open;
        let disclosure = disclosure_row(
            SharedString::from(format!("comp-mind-disc-{id}")),
            "詳細設定（性格の側面・セリフ例・背景・表現のクセ）",
            mind_open,
            theme,
            move |_, cx| {
                studio_disc.update(cx, |this, cx| {
                    this.mind_disclosure_open = !this.mind_disclosure_open;
                    cx.notify();
                });
            },
        );

        let mind_strip = if mind_open {
            let profile = mind_view
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
                    profile.core.clone()
                } else {
                    mind_val.clone()
                },
                revision: prev,
                facets: split_pipe(facets_d.read(cx).text()),
                examples: split_pipe(examples_d.read(cx).text()),
                background: split_pipe(bg_d.read(cx).text()),
                expression: split_pipe(expr_d.read(cx).text()),
                ..Default::default()
            };
            let comp_id_save = id.clone();
            let hub_mind = self.hub.clone();

            Some(
                div()
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
                        Some(Icon::Check),
                        Tone::Soft,
                        live,
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
                    ))),
            )
        } else {
            None
        };

        let armed = self.delete_armed_companion.as_deref() == Some(id.as_str());
        let studio_arm = cx.entity();
        let studio_disarm = cx.entity();
        let hub_del = self.hub.clone();
        let hub_save = self.hub.clone();
        let arm_id = id.clone();
        let del_id = id.clone();

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
                    None,
                    Tone::Ghost,
                    true,
                    theme,
                    move |_, cx| {
                        studio_disarm.update(cx, |this, cx| {
                            this.delete_armed_companion = None;
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    SharedString::from(format!("comp-del-exec-{id}")),
                    "削除を実行",
                    Some(Icon::Trash),
                    Tone::Danger,
                    live,
                    theme,
                    move |_, cx| {
                        hub_del.update(cx, |hub, cx| hub.delete_companion(del_id.clone(), cx));
                    },
                ))
        } else {
            div().child(pill_button(
                SharedString::from(format!("comp-del-arm-{id}")),
                "削除",
                Some(Icon::Trash),
                Tone::Danger,
                live,
                theme,
                move |_, cx| {
                    studio_arm.update(cx, |this, cx| {
                        this.delete_armed_companion = Some(arm_id.clone());
                        cx.notify();
                    });
                },
            ))
        };

        let export_id = id.clone();
        let hub_export = self.hub.clone();
        let save_id = id.clone();

        let model_d_pick = body_model_d.clone();
        let model_d_default = body_model_d.clone();
        let model_field = div()
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
                                None,
                                Tone::Ghost,
                                true,
                                theme,
                                move |_, cx| {
                                    model_d_default
                                        .update(cx, |d, cx| d.set("ENE.vrm".to_owned(), cx));
                                },
                            ))
                            .child(pill_button(
                                SharedString::from(format!("comp-model-browse-{id}")),
                                "ファイルを選択",
                                None,
                                Tone::Soft,
                                true,
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
            .child(body_model_d);

        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(labeled_field("名前", name_d, theme))
            .child(labeled_field("人格の核", mind_d, theme))
            .child(labeled_field("話し方", persona_d, theme))
            .child(model_field)
            .child(voice_selector)
            .child(disclosure)
            .when_some(mind_strip, |this, strip| this.child(strip))
            .when_some(exported_json, |this, json| {
                this.child(
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
                        ),
                )
            })
            .child(
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
                                None,
                                Tone::Ghost,
                                live,
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
                                Some(Icon::Check),
                                Tone::Primary,
                                live && !name_val.is_empty(),
                                theme,
                                move |_, cx| {
                                    hub_save.update(cx, |hub, cx| {
                                        hub.update_companion_profile(
                                            save_id.clone(),
                                            name_val.clone(),
                                            persona_val.clone(),
                                            mind_val.clone(),
                                            next_body.clone(),
                                            next_voice.clone(),
                                            cx,
                                        );
                                    });
                                },
                            )),
                    ),
            )
            .into_any_element()
    }
}

fn parse_voice_selection(raw_key: &str, manual_fallback: &str) -> (Option<String>, Option<String>) {
    let trimmed = raw_key.trim();
    if let Some((cid, vid)) = trimmed.split_once('\0') {
        let conn = (!cid.trim().is_empty()).then(|| cid.trim().to_owned());
        let voice = (!vid.trim().is_empty()).then(|| vid.trim().to_owned());
        return (conn, voice);
    }
    let fallback = manual_fallback.trim();
    if fallback.is_empty() {
        (None, None)
    } else {
        (None, Some(fallback.to_owned()))
    }
}

fn split_pipe(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b11_parse_voice_selection_preserves_connection_and_duplicate_voice_id() {
        assert_eq!(
            parse_voice_selection("tts-b\0shared-id", ""),
            (Some("tts-b".into()), Some("shared-id".into()))
        );
        assert_eq!(
            parse_voice_selection("", "manual-3"),
            (None, Some("manual-3".into()))
        );
        assert_eq!(parse_voice_selection("", ""), (None, None));
    }
}
