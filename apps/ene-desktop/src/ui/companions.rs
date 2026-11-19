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
    pub(crate) fn render_companions(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (live, companions, active_id, has_models, tts_voices, tts_conn_ids, minds) = {
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
                !hub.model_choices().is_empty(),
                hub.tts_voice_choices(),
                tts_ids,
                hub.model.minds.clone(),
            )
        };

        let studio_toggle_create = cx.entity();
        let creating = self.creating_companion;
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
                        cx.notify();
                    });
                },
            ));

        let create_panel = if creating {
            Some(self.render_create_panel(live, has_models, theme, cx))
        } else {
            None
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        for comp in companions {
            let id = comp.id.clone();
            let rev = comp.revision;
            let active = active_id.as_deref() == Some(id.as_str());
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
                            active.then_some(theme.mint),
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
                                        .line_clamp(2)
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
                        .gap(px(8.))
                        .child(if active {
                            status_pill("使用中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
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
                                    hub_expand.update(cx, |hub, cx| {
                                        if hub.model.companion_id() != Some(target_id.as_str()) {
                                            hub.select_companion(&target_id, cx);
                                        } else {
                                            hub.load_mind(cx);
                                        }
                                    });
                                }
                            },
                        )),
                );

            let editor = if expanded {
                Some(self.render_companion_editor(
                    &comp,
                    rev,
                    minds.get(&id),
                    &tts_voices,
                    &tts_conn_ids,
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
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(create_panel, |this, p| this.child(p))
            .child(div().flex().flex_col().gap(px(10.)).children(rows))
            .into_any_element()
    }

    fn render_create_panel(
        &mut self,
        live: bool,
        has_models: bool,
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
        let studio_cancel = cx.entity();

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
            .child(labeled_field("名前", name_d, theme))
            .child(labeled_field("人格の核", mind_d, theme))
            .child(labeled_field("話し方", persona_d, theme))
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
                                cx.notify();
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
    fn render_companion_editor(
        &mut self,
        comp: &w::Companion,
        rev: u64,
        mind_view: Option<&w::MindView>,
        tts_voices: &[(String, String, String)],
        tts_conn_ids: &[String],
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = comp.id.clone();
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
        let voice_d = self.draft(
            cx,
            format!("comp:{id}:r{rev}:voice"),
            "話者ID（任意）",
            comp.voice_identifier.as_deref(),
            theme,
        );

        let name_val = name_d.read(cx).text().trim().to_owned();
        let mind_val = mind_d.read(cx).text().trim().to_owned();
        let persona_val = persona_d.read(cx).text().trim().to_owned();
        let voice_val = voice_d.read(cx).text().trim().to_owned();

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
        for (vid, label, conn_name) in tts_voices {
            voice_options.push(DropdownOption {
                value: vid.clone(),
                label: label.clone(),
                detail: Some(format!("{conn_name} / ID: {vid}")),
            });
        }
        let selected_voice_label = if voice_val.is_empty() {
            Some("接続の既定を使用".to_owned())
        } else {
            tts_voices
                .iter()
                .find(|(vid, _, _)| vid == &voice_val)
                .map(|(vid, label, _)| format!("{label} (ID: {vid})"))
                .or_else(|| Some(voice_val.clone()))
        };
        let studio_voice_toggle = cx.entity();
        let studio_voice_pick = cx.entity();
        let toggle_key = voice_dd_key.clone();
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
            .gap(px(4.))
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
                            .child("声（話者）"),
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
                Some(voice_val.as_str()),
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
                    voice_d_pick.update(cx, |d, cx| d.set(picked, cx));
                    studio_voice_pick.update(cx, |this, cx| {
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ))
            .when(tts_voices.is_empty(), |this| this.child(voice_d));

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
            .child(voice_selector)
            .child(disclosure)
            .when_some(mind_strip, |this, strip| this.child(strip))
            .child(
                div()
                    .pt(px(4.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(delete_controls)
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
                                    name_val.clone(),
                                    persona_val.clone(),
                                    mind_val.clone(),
                                    voice_val.clone(),
                                    cx,
                                );
                            });
                        },
                    )),
            )
            .into_any_element()
    }
}

fn split_pipe(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
