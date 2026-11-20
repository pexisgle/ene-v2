//! Connections chapter in `SettingsStudio`: single-line 62px rows with status pills,
//! 1-level hairline expansion, dynamic `options_schema`, MCP tool selector,
//! and bottom collapsed disclosure for model-role & voice slot overrides.

use super::icons::Icon;
use super::mcp_add::parse_process_args;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{
    DropdownOption, Tone, circle_icon_button, disclosure_row, dropdown_select, kind_pill,
    pill_button, status_pill, toggle_switch,
};
use super::{ModelRoleSlot, SettingsStudio};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

impl SettingsStudio {
    pub(crate) fn render_connections(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (live, connections, providers, checks, companion, all_models, embedding_models) = {
            let hub = self.hub.read(cx);
            (
                hub.live(),
                hub.model
                    .settings()
                    .map(|s| s.connections.clone())
                    .unwrap_or_default(),
                hub.model.providers.clone().unwrap_or_default(),
                hub.model.checks.clone(),
                hub.model.companion().cloned(),
                hub.all_llm_models(),
                hub.all_embedding_models(),
            )
        };

        if let Some(id) = self.pending_mcp.as_ref()
            && connections.iter().any(|connection| &connection.id == id)
        {
            let id = self
                .pending_mcp
                .take()
                .expect("the new MCP connection is saved");
            self.clear_mcp_drafts();
            let hub = self.hub.clone();
            cx.defer(move |cx| hub.update(cx, |hub, cx| hub.check_connection(id, cx)));
        }
        let studio_add = cx.entity();
        let studio_mcp = cx.entity();
        let adding = self.adding_connection;
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
                    .child("接続"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(pill_button(
                        "conn-add-mcp",
                        "MCP を追加",
                        Some(Icon::Plus),
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_mcp.update(cx, |this, cx| {
                                this.adding_connection = true;
                                this.custom_connection_open = true;
                                this.custom_kind = w::ConnectionKind::Mcp;
                                this.mcp_add_mode = super::mcp_add::Mode::Npx;
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        "conn-toggle-add",
                        if adding {
                            "閉じる"
                        } else {
                            "接続を追加"
                        },
                        (!adding).then_some(Icon::Plus),
                        if adding { Tone::Ghost } else { Tone::Primary },
                        true,
                        theme,
                        move |_, cx| {
                            studio_add.update(cx, |this, cx| {
                                this.adding_connection = !this.adding_connection;
                                if this.adding_connection
                                    && this.custom_kind == w::ConnectionKind::Mcp
                                {
                                    this.custom_kind = w::ConnectionKind::Llm;
                                }
                                cx.notify();
                            });
                        },
                    )),
            );

        let add_panel = if adding {
            Some(self.render_add_connection_panel(live, &providers, theme, cx))
        } else {
            None
        };

        let search_card = self.render_web_search_card(&connections, &checks, live, theme, cx);

        let mut rows: Vec<AnyElement> = Vec::new();
        for conn in &connections {
            let check = if super::llm_endpoint(conn)
                .is_some_and(|ep| ep.adapter == super::GOOGLE_AGENT_PLATFORM)
            {
                conn.last_check.as_option()
            } else {
                checks.get(&conn.id).or_else(|| conn.last_check.as_option())
            };
            rows.push(self.render_connection_row(conn, check, live, theme, cx));
        }
        if rows.is_empty() && !adding {
            rows.push(
                div()
                    .w_full()
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(14.))
                    .px(px(16.))
                    .py(px(18.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("登録されている接続はありません。右上の「接続を追加」から設定できます。")
                    .into_any_element(),
            );
        }

        let studio_adv = cx.entity();
        let adv_open = self.advanced_roles_open;
        let adv_toggle = disclosure_row(
            "conn-advanced-roles-toggle",
            "詳細設定（役割別のモデル割当・エンドポイント）",
            adv_open,
            theme,
            move |_, cx| {
                studio_adv.update(cx, |this, cx| {
                    this.advanced_roles_open = !this.advanced_roles_open;
                    cx.notify();
                });
            },
        );
        let adv_panel = if adv_open {
            Some(self.render_advanced_roles_panel(
                companion.as_ref(),
                &all_models,
                &embedding_models,
                live,
                theme,
                cx,
            ))
        } else {
            None
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(add_panel, |this, p| this.child(p))
            .child(search_card)
            .when(!rows.is_empty(), |this| {
                this.child(div().w_full().flex().flex_col().gap(px(10.)).children(rows))
            })
            .child(
                div()
                    .pt(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(adv_toggle)
                    .when_some(adv_panel, |this, p| this.child(p)),
            )
            .into_any_element()
    }

    fn render_web_search_card(
        &mut self,
        connections: &[w::Connection],
        checks: &std::collections::BTreeMap<String, w::ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let search_conns: Vec<&w::Connection> = connections
            .iter()
            .filter(|c| c.kind == w::ConnectionKind::Search)
            .collect();
        let active_conn = search_conns.iter().copied().find(|c| {
            c.enabled
                && (!super::search_adapter_of(c).is_some_and(super::search_adapter_requires_key)
                    || matches!(c.credential_state.as_str(), "registered" | "environment"))
        });
        let active_fallback =
            active_conn.or_else(|| search_conns.iter().copied().find(|c| c.enabled));

        let (summary_label, summary_dot, summary_bg, summary_fg) = match active_conn {
            Some(conn) => (
                format!("使用中: {}", conn.name),
                theme.mint,
                theme.mint_soft,
                theme.mint_fg,
            ),
            None => match active_fallback {
                Some(conn) => (
                    format!("{}（キー登録が必要）", conn.name),
                    theme.amber,
                    theme.amber_soft,
                    theme.amber_fg,
                ),
                None => (
                    "未設定（検索オフ）".into(),
                    theme.ink_placeholder,
                    theme.bg_blush,
                    theme.ink_muted,
                ),
            },
        };

        let mut provider_rows: Vec<AnyElement> = Vec::new();
        for (adapter, display_name, placeholder, env_hint) in [
            ("exa", "Exa", "Exa API キーを入力…", "EXA_API_KEY"),
            (
                "tavily",
                "Tavily",
                "Tavily API キーを入力… (tvly-...)",
                "TAVILY_API_KEY",
            ),
        ] {
            let existing = search_conns.iter().copied().find(|c| {
                super::search_adapter_of(c).is_some_and(|a| a.eq_ignore_ascii_case(adapter))
            });
            let conn_id = existing.map(|c| c.id.clone());
            let has_cred = existing.is_some_and(|c| {
                matches!(c.credential_state.as_str(), "registered" | "environment")
            });
            let is_active = existing.is_some_and(|c| c.enabled);
            let is_checking = conn_id
                .as_ref()
                .is_some_and(|id| self.hub.read(cx).checking_connections.contains(id));
            let check =
                existing.and_then(|c| checks.get(&c.id).or_else(|| c.last_check.as_option()));

            let reveal_key = format!("search-card:{adapter}");
            let revealed = self.revealed_secret_for.as_deref() == Some(reveal_key.as_str());
            let draft = self.masked_draft(
                cx,
                format!("search-card:secret:{adapter}"),
                placeholder,
                !revealed,
                theme,
            );
            let key_text = draft.read(cx).text().trim().to_owned();

            let (cred_label, cred_bg, cred_fg) = match existing.map(|c| c.credential_state.as_str())
            {
                Some("registered") => ("キー登録済み", theme.mint_soft, theme.mint_fg),
                Some("environment") => ("環境変数", theme.mint_soft, theme.mint_fg),
                _ => ("未登録", theme.bg_surface, theme.ink_muted),
            };
            let check_badge = if is_checking {
                Some(("確認中…", theme.amber_soft, theme.amber_fg))
            } else {
                match check.map(|c| c.healthy) {
                    Some(true) => Some(("確認済み", theme.mint_soft, theme.mint_fg)),
                    Some(false) => Some(("接続エラー", theme.danger_soft, theme.danger_fg)),
                    None => None,
                }
            };
            let err_msg = check
                .filter(|c| !c.healthy)
                .and_then(|c| c.failure.as_option())
                .map(|f| f.message.trim().to_owned())
                .filter(|m| !m.is_empty());

            let studio_eye = cx.entity();
            let reveal_key_click = reveal_key.clone();
            let hub_save = self.hub.clone();
            let key_for_save = key_text.clone();
            let hub_use = self.hub.clone();
            let use_id = conn_id.clone();
            let hub_check = self.hub.clone();
            let check_id = conn_id.clone();

            provider_rows.push(
                div()
                    .w_full()
                    .rounded(px(10.))
                    .bg(theme.bg_blush)
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .flex_wrap()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink)
                                            .child(display_name),
                                    )
                                    .child(status_pill(cred_label, None, cred_bg, cred_fg))
                                    .when_some(check_badge, |this, (label, bg, fg)| {
                                        this.child(status_pill(label, None, bg, fg))
                                    })
                                    .when(is_active && has_cred, |this| {
                                        this.child(status_pill(
                                            "使用中",
                                            Some(theme.mint),
                                            theme.mint_soft,
                                            theme.mint_fg,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.))
                                    .when_some(
                                        use_id.filter(|_| has_cred && !is_active),
                                        |this, cid| {
                                            this.child(pill_button(
                                                SharedString::from(format!(
                                                    "search-card-use-{adapter}"
                                                )),
                                                "この検索を使う",
                                                Some(Icon::Check),
                                                Tone::Soft,
                                                live,
                                                theme,
                                                move |_, cx| {
                                                    hub_use.update(cx, |hub, cx| {
                                                        hub.assign_search_connection(
                                                            Some(cid.clone()),
                                                            cx,
                                                        );
                                                    });
                                                },
                                            ))
                                        },
                                    )
                                    .when_some(check_id.filter(|_| has_cred), |this, cid| {
                                        this.child(pill_button(
                                            SharedString::from(format!(
                                                "search-card-check-{adapter}"
                                            )),
                                            if is_checking {
                                                "確認中…"
                                            } else {
                                                "確認"
                                            },
                                            Some(Icon::Refresh),
                                            Tone::Ghost,
                                            live && !is_checking,
                                            theme,
                                            move |_, cx| {
                                                hub_check.update(cx, |hub, cx| {
                                                    hub.check_connection(cid.clone(), cx);
                                                });
                                            },
                                        ))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex_1().min_w_0().child(draft))
                            .child(circle_icon_button(
                                SharedString::from(format!("search-card-eye-{adapter}")),
                                if revealed { Icon::EyeOff } else { Icon::Eye },
                                30.,
                                14.,
                                Tone::Ghost,
                                true,
                                theme,
                                move |_, cx| {
                                    studio_eye.update(cx, |this, cx| {
                                        this.revealed_secret_for = if revealed {
                                            None
                                        } else {
                                            Some(reveal_key_click.clone())
                                        };
                                        cx.notify();
                                    });
                                },
                            ))
                            .child(pill_button(
                                SharedString::from(format!("search-card-save-{adapter}")),
                                if has_cred {
                                    "キーを更新"
                                } else {
                                    "登録して使う"
                                },
                                Some(Icon::Check),
                                Tone::Primary,
                                live && !key_text.is_empty(),
                                theme,
                                move |_, cx| {
                                    hub_save.update(cx, |hub, cx| {
                                        hub.save_search_provider_key(
                                            adapter,
                                            key_for_save.clone(),
                                            true,
                                            cx,
                                        );
                                    });
                                },
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child(format!("環境変数 {env_hint} でも設定できます")),
                    )
                    .when_some(err_msg, |this, msg| {
                        this.child(
                            div()
                                .rounded(px(8.))
                                .bg(theme.danger_soft)
                                .px(px(10.))
                                .py(px(6.))
                                .text_xs()
                                .text_color(theme.danger_fg)
                                .child(msg),
                        )
                    })
                    .into_any_element(),
            );
        }

        // Quick active-service switcher when at least one search connection exists.
        let switcher = (!search_conns.is_empty()).then(|| {
            let mut chips: Vec<AnyElement> = Vec::new();
            for conn in &search_conns {
                let cid = conn.id.clone();
                let selected = active_fallback.is_some_and(|a| a.id == conn.id);
                let hub_sw = self.hub.clone();
                chips.push(
                    pill_button(
                        SharedString::from(format!("search-switch-{}", conn.id)),
                        conn.name.clone(),
                        selected.then_some(Icon::Check),
                        if selected { Tone::Primary } else { Tone::Ghost },
                        live,
                        theme,
                        move |_, cx| {
                            hub_sw.update(cx, |hub, cx| {
                                hub.assign_search_connection(Some(cid.clone()), cx);
                            });
                        },
                    )
                    .into_any_element(),
                );
            }
            let hub_off = self.hub.clone();
            let is_off = active_fallback.is_none();
            chips.push(
                pill_button(
                    "search-switch-off",
                    "オフ",
                    is_off.then_some(Icon::Check),
                    if is_off { Tone::Soft } else { Tone::Ghost },
                    live,
                    theme,
                    move |_, cx| {
                        hub_off.update(cx, |hub, cx| {
                            hub.assign_search_connection(None, cx);
                        });
                    },
                )
                .into_any_element(),
            );
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink_muted)
                        .child("使用する検索サービス"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .children(chips),
                )
        });

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.ink)
                                    .child("Web 検索（Exa / Tavily）"),
                            ),
                    )
                    .child(status_pill(
                        summary_label,
                        Some(summary_dot),
                        summary_bg,
                        summary_fg,
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("Exa や Tavily の API キーを登録して使用する検索サービスを選べます（Mwmbl・SearXNG・Brave は「接続を追加」からも設定できます）。"),
            )
            .when_some(switcher, |this, sw| this.child(sw))
            .children(provider_rows)
            .into_any_element()
    }

    fn render_add_connection_panel(
        &mut self,
        live: bool,
        providers: &[w::LlmProvider],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.custom_connection_open && self.custom_kind == w::ConnectionKind::Mcp {
            return self.render_mcp_add(live, theme, cx);
        }
        let mut llm_chips: Vec<AnyElement> = Vec::new();
        for prov in providers {
            let p = prov.clone();
            let hub = self.hub.clone();
            let studio = cx.entity();
            llm_chips.push(pill_button(
                SharedString::from(format!("add-llm-prov-{}", prov.id)),
                prov.name.clone(),
                Some(Icon::Plus),
                Tone::Soft,
                live,
                theme,
                move |_, cx| {
                    hub.update(cx, |hub, cx| {
                        hub.add_llm_from_provider(p.clone(), vec![], cx)
                    });
                    studio.update(cx, |this, cx| {
                        this.adding_connection = false;
                        cx.notify();
                    });
                },
            ));
        }

        let hub_vv = self.hub.clone();
        let hub_sakura_tts = self.hub.clone();
        let hub_oai_tts = self.hub.clone();
        let hub_sakura_stt = self.hub.clone();
        let hub_oai_stt = self.hub.clone();
        let hub_stt = self.hub.clone();
        let hub_sakura_emb = self.hub.clone();
        let hub_oai_emb = self.hub.clone();
        let hub_search = self.hub.clone();
        let studio_vv = cx.entity();
        let studio_sakura_tts = cx.entity();
        let studio_oai = cx.entity();
        let studio_sakura_stt = cx.entity();
        let studio_oai_stt = cx.entity();
        let studio_stt = cx.entity();
        let studio_sakura_emb = cx.entity();
        let studio_oai_emb = cx.entity();
        let studio_search = cx.entity();

        let voice_chips = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .child(pill_button(
                "add-preset-voicevox",
                "VOICEVOX（ローカル）",
                Some(Icon::Volume),
                Tone::Mint,
                live,
                theme,
                move |_, cx| {
                    hub_vv.update(cx, |hub, cx| hub.add_voicevox_preset(3, None, cx));
                    studio_vv.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some("voicevox".into());
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-sakura-tts",
                "さくらのAI Engine TTS",
                Some(Icon::Volume),
                Tone::Soft,
                live,
                theme,
                move |_, cx| {
                    hub_sakura_tts.update(cx, |hub, cx| hub.add_sakura_tts_preset(cx));
                    studio_sakura_tts.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some("sakura-tts".into());
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-openai-tts",
                "OpenAI互換 TTS",
                Some(Icon::Volume),
                Tone::Ghost,
                live,
                theme,
                move |_, cx| {
                    hub_oai_tts.update(cx, |hub, cx| {
                        hub.add_openai_tts_preset(
                            "https://api.openai.com/v1".into(),
                            "tts-1".into(),
                            "alloy".into(),
                            cx,
                        );
                    });
                    studio_oai.update(cx, |this, cx| {
                        this.adding_connection = false;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-sakura-stt",
                "さくらのAI Engine STT",
                Some(Icon::Mic),
                Tone::Soft,
                live,
                theme,
                move |_, cx| {
                    hub_sakura_stt.update(cx, |hub, cx| hub.add_sakura_stt_preset(cx));
                    studio_sakura_stt.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some("sakura-stt".into());
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-openai-stt",
                "OpenAI 音声認識（STT）",
                Some(Icon::Mic),
                Tone::Ghost,
                live,
                theme,
                move |_, cx| {
                    hub_oai_stt.update(cx, |hub, cx| {
                        hub.add_stt_preset(
                            "OpenAI 音声認識".into(),
                            "https://api.openai.com/v1".into(),
                            "gpt-4o-mini-transcribe".into(),
                            "ja".into(),
                            cx,
                        );
                    });
                    studio_oai_stt.update(cx, |this, cx| {
                        this.adding_connection = false;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-whisper-stt",
                "Whisper STT（ローカル）",
                Some(Icon::Mic),
                Tone::Ghost,
                live,
                theme,
                move |_, cx| {
                    hub_stt.update(cx, |hub, cx| {
                        hub.add_stt_preset(
                            "ローカル Whisper".into(),
                            "http://127.0.0.1:8080/v1".into(),
                            "whisper-1".into(),
                            "ja".into(),
                            cx,
                        );
                    });
                    studio_stt.update(cx, |this, cx| {
                        this.adding_connection = false;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-sakura-embedding",
                "さくらのAI Engine Embedding",
                Some(Icon::Search),
                Tone::Soft,
                live,
                theme,
                move |_, cx| {
                    hub_sakura_emb.update(cx, |hub, cx| hub.add_sakura_embedding_preset(cx));
                    studio_sakura_emb.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some("sakura-embedding".into());
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ))
            .child(pill_button(
                "add-preset-openai-embedding",
                "OpenAI Embedding",
                Some(Icon::Search),
                Tone::Ghost,
                live,
                theme,
                move |_, cx| {
                    hub_oai_emb.update(cx, |hub, cx| hub.add_openai_embedding_preset(cx));
                    studio_oai_emb.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some("openai-embedding".into());
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ));

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
        let mut search_chips = div().flex().flex_row().flex_wrap().gap(px(6.));
        for (btn_id, preset_id, label, conn_name, adapter, url, tone) in search_presets {
            let hub_s = hub_search.clone();
            let studio_s = studio_search.clone();
            search_chips = search_chips.child(pill_button(
                btn_id,
                label,
                Some(Icon::Search),
                tone,
                live,
                theme,
                move |_, cx| {
                    let target_id = hub_s.update(cx, |hub, cx| {
                        hub.add_search_preset(preset_id, conn_name, adapter, url, true, cx)
                    });
                    studio_s.update(cx, |this, cx| {
                        this.adding_connection = false;
                        this.expanded_connection_id = Some(target_id);
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            ));
        }

        let custom_kind = self.custom_kind;
        let mut kind_chips = Vec::new();
        for kind in [
            w::ConnectionKind::Llm,
            w::ConnectionKind::Tts,
            w::ConnectionKind::Stt,
            w::ConnectionKind::Embedding,
            w::ConnectionKind::Mcp,
            w::ConnectionKind::Acp,
            w::ConnectionKind::Search,
        ] {
            let selected = custom_kind == kind;
            let studio = cx.entity();
            kind_chips.push(pill_button(
                SharedString::from(format!("add-custom-kind-{kind:?}")),
                kind_label(kind),
                None,
                if selected { Tone::Primary } else { Tone::Ghost },
                true,
                theme,
                move |_, cx| {
                    studio.update(cx, |this, cx| {
                        this.custom_kind = kind;
                        cx.notify();
                    });
                },
            ));
        }

        let (adapter_label, adapter_placeholder, target_label, target_placeholder) =
            match custom_kind {
                w::ConnectionKind::Search => (
                    "検索アダプタ",
                    "例: exa / tavily / brave / mwmbl / searxng",
                    "検索 API URL",
                    "例: https://api.exa.ai/search",
                ),
                w::ConnectionKind::Acp | w::ConnectionKind::Mcp => (
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
            };
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
                                .child("AIプロバイダを追加"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(6.))
                                .children(llm_chips),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink_muted)
                            .child("音声・埋め込み機能を追加"),
                    )
                    .child(voice_chips),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink_muted)
                            .child("Web 検索を追加"),
                    )
                    .child(search_chips),
            )
            .child(disclosure_row(
                "custom-connection",
                "カスタム接続",
                self.custom_connection_open,
                theme,
                {
                    let studio = cx.entity();
                    move |_, cx| {
                        studio.update(cx, |this, cx| {
                            this.custom_connection_open = !this.custom_connection_open;
                            cx.notify();
                        })
                    }
                },
            ))
            .when(self.custom_connection_open, |panel| {
                panel.child(
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
                        .child(div().flex().flex_row().justify_end().child(pill_button(
                            "add-custom-submit",
                            "追加",
                            Some(Icon::Plus),
                            Tone::Primary,
                            live && !c_name.is_empty(),
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
                                    this.adding_connection = false;
                                    cx.notify();
                                });
                            },
                        ))),
                )
            })
            .into_any_element()
    }

    fn render_connection_row(
        &mut self,
        conn: &w::Connection,
        check: Option<&w::ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = conn.id.clone();
        let kind = conn
            .kind
            .as_known()
            .unwrap_or(w::ConnectionKind::Unspecified);
        let expanded = self.expanded_connection_id.as_deref() == Some(id.as_str());

        let needs_key_flag = super::needs_key(
            conn,
            self.hub
                .read(cx)
                .model
                .providers
                .as_deref()
                .unwrap_or_default(),
        );
        let is_checking = self.hub.read(cx).checking_connections.contains(&id);
        let is_voicevox = kind == w::ConnectionKind::Tts
            && !super::is_sakura_conn(conn)
            && extract_endpoint(conn.config.as_option().and_then(|c| c.config.as_ref()))
                .is_some_and(|ep| ep.adapter.eq_ignore_ascii_case("voicevox"));
        let is_unsupported_check = check.is_some_and(|c| {
            !c.healthy
                && c.failure
                    .as_option()
                    .is_some_and(|f| f.code == "unsupported")
        });
        let (state_label, state_dot, state_bg, state_fg) = if !conn.enabled {
            (
                "無効",
                theme.ink_placeholder,
                theme.bg_blush,
                theme.ink_muted,
            )
        } else if needs_key_flag {
            (
                "キー登録が必要",
                theme.amber,
                theme.amber_soft,
                theme.amber_fg,
            )
        } else if is_unsupported_check {
            ("設定済み", theme.mint, theme.mint_soft, theme.mint_fg)
        } else {
            match check.map(|c| c.healthy) {
                Some(true) => (
                    "接続先を確認済み",
                    theme.mint,
                    theme.mint_soft,
                    theme.mint_fg,
                ),
                Some(false) => (
                    "接続エラー",
                    theme.danger,
                    theme.danger_soft,
                    theme.danger_fg,
                ),
                None => ("未確認", theme.amber, theme.amber_soft, theme.amber_fg),
            }
        };

        let hub_check = self.hub.clone();
        let hub_expand_probe = self.hub.clone();
        let check_id = id.clone();
        let studio_expand = cx.entity();
        let expand_id = id.clone();
        let agent_platform =
            super::llm_endpoint(conn).is_some_and(|ep| ep.adapter == super::GOOGLE_AGENT_PLATFORM);
        let should_auto_probe = conn.enabled
            && !needs_key_flag
            && (kind == w::ConnectionKind::Llm || is_voicevox)
            && check.is_none_or(|c| {
                c.resources.is_empty()
                    || (agent_platform
                        && !c
                            .capabilities
                            .iter()
                            .any(|cap| cap == "verified_model_list"))
            });

        let header = div()
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
                    .gap(px(10.))
                    .child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .truncate()
                            .child(conn.name.clone()),
                    )
                    .child(kind_pill(kind_label(kind), theme))
                    .child(status_pill(
                        state_label,
                        Some(state_dot),
                        state_bg,
                        state_fg,
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(pill_button(
                        SharedString::from(format!("conn-check-{id}")),
                        if is_checking && is_voicevox {
                            "起動・確認中…"
                        } else if is_checking {
                            "確認中…"
                        } else {
                            "確認"
                        },
                        Some(Icon::Refresh),
                        Tone::Ghost,
                        live && !is_checking,
                        theme,
                        move |_, cx| {
                            hub_check.update(cx, |hub, cx| {
                                hub.check_connection(check_id.clone(), cx);
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("conn-edit-{id}")),
                        if expanded {
                            "閉じる"
                        } else if state_label == "キー登録が必要" {
                            "キーを登録"
                        } else {
                            "編集"
                        },
                        None,
                        if expanded { Tone::Soft } else { Tone::Ghost },
                        true,
                        theme,
                        move |_, cx| {
                            let mut probe_id = None;
                            studio_expand.update(cx, |this, cx| {
                                if this.expanded_connection_id.as_deref()
                                    == Some(expand_id.as_str())
                                {
                                    this.expanded_connection_id = None;
                                } else {
                                    this.expanded_connection_id = Some(expand_id.clone());
                                    if should_auto_probe
                                        && this.auto_probed_connections.insert(expand_id.clone())
                                    {
                                        probe_id = Some(expand_id.clone());
                                    }
                                }
                                this.open_dropdown = None;
                                this.revoke_armed_for = None;
                                cx.notify();
                            });
                            if let Some(pid) = probe_id {
                                hub_expand_probe.update(cx, |hub, cx| {
                                    hub.check_connection(pid, cx);
                                });
                            }
                        },
                    )),
            );

        let detail = if expanded {
            Some(self.render_connection_editor(conn, check, live, theme, cx))
        } else {
            None
        };

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .flex()
            .flex_col()
            .child(header)
            .when_some(detail, |this, d| this.child(d))
            .into_any_element()
    }

    fn render_connection_editor(
        &mut self,
        conn: &w::Connection,
        check: Option<&w::ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = conn.id.clone();
        let rev = conn.revision;
        let conn_toggle = conn.clone();
        let hub_toggle = self.hub.clone();
        let hub_delete = self.hub.clone();
        let delete_id = id.clone();
        let delete_title = format!("「{}」を削除しますか？", conn.name);

        let err_strip = check
            .filter(|c| !c.healthy)
            .and_then(|c| c.failure.as_option())
            .filter(|f| f.code != "unsupported" && !f.message.trim().is_empty())
            .map(|f| {
                div()
                    .rounded(px(10.))
                    .bg(theme.danger_soft)
                    .px(px(12.))
                    .py(px(8.))
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child(f.message.clone())
            });

        // Credentials strip
        let revealed = self.revealed_secret_for.as_deref() == Some(id.as_str());
        let secret_d = self.masked_draft(
            cx,
            format!("conn:secret:{id}"),
            "新しいAPIキー / トークン",
            !revealed,
            theme,
        );
        let secret_val = secret_d.read(cx).text().trim().to_owned();
        let has_cred =
            conn.credential_state == "registered" || conn.credential_state == "environment";
        let cred_label = match conn.credential_state.as_str() {
            "registered" => "登録済み",
            "environment" => "環境変数",
            _ => "未登録",
        };
        let studio_eye = cx.entity();
        let eye_id = id.clone();
        let hub_reg = self.hub.clone();
        let reg_id = id.clone();
        let revoke_armed = self.revoke_armed_for.as_deref() == Some(id.as_str());
        let studio_rev_arm = cx.entity();
        let studio_rev_cancel = cx.entity();
        let hub_rev = self.hub.clone();
        let arm_id = id.clone();
        let rev_id = id.clone();

        let sakura_conn_ids: Vec<String> = if super::is_sakura_conn(conn) {
            self.hub
                .read(cx)
                .model
                .settings()
                .map(|s| {
                    s.connections
                        .iter()
                        .filter(|c| super::is_sakura_conn(c))
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let has_other_sakura = sakura_conn_ids.len() > 1;
        let hub_reg_all = self.hub.clone();
        let secret_val_all = secret_val.clone();
        let cred_strip = div()
            .rounded(px(10.))
            .bg(theme.bg_blush)
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child("APIキー・認証情報"),
                    )
                    .child(status_pill(
                        cred_label,
                        None,
                        if has_cred {
                            theme.mint_soft
                        } else {
                            theme.bg_surface
                        },
                        if has_cred {
                            theme.mint_fg
                        } else {
                            theme.ink_muted
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w_0().child(secret_d))
                    .child(circle_icon_button(
                        SharedString::from(format!("conn-eye-{id}")),
                        if revealed { Icon::EyeOff } else { Icon::Eye },
                        30.,
                        14.,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_eye.update(cx, |this, cx| {
                                this.revealed_secret_for =
                                    if revealed { None } else { Some(eye_id.clone()) };
                                cx.notify();
                            });
                        },
                    ))
                    .when(has_other_sakura, |this| {
                        let ids_all = sakura_conn_ids.clone();
                        let sec_all = secret_val_all.clone();
                        let hub_all = hub_reg_all.clone();
                        this.child(pill_button(
                            SharedString::from(format!("conn-save-sakura-all-{id}")),
                            "全さくら接続に登録",
                            Some(Icon::Check),
                            Tone::Soft,
                            live && !sec_all.is_empty(),
                            theme,
                            move |_, cx| {
                                hub_all.update(cx, |hub, cx| {
                                    hub.register_secret_for_connections(
                                        ids_all.clone(),
                                        sec_all.clone(),
                                        cx,
                                    );
                                });
                            },
                        ))
                    })
                    .child(pill_button(
                        SharedString::from(format!("conn-save-key-{id}")),
                        "キーを登録",
                        Some(Icon::Check),
                        Tone::Primary,
                        live && !secret_val.is_empty(),
                        theme,
                        move |_, cx| {
                            hub_reg.update(cx, |hub, cx| {
                                hub.register_secret(reg_id.clone(), secret_val.clone(), cx);
                            });
                        },
                    )),
            )
            .when(conn.credential_state == "registered", |this| {
                this.child(div().flex().flex_row().justify_end().gap(px(6.)).child(
                    if revoke_armed {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(pill_button(
                                SharedString::from(format!("conn-rev-cancel-{id}")),
                                "キャンセル",
                                None,
                                Tone::Ghost,
                                true,
                                theme,
                                move |_, cx| {
                                    studio_rev_cancel.update(cx, |this, cx| {
                                        this.revoke_armed_for = None;
                                        cx.notify();
                                    });
                                },
                            ))
                            .child(pill_button(
                                SharedString::from(format!("conn-rev-exec-{id}")),
                                "キーを削除",
                                Some(Icon::Trash),
                                Tone::Danger,
                                live,
                                theme,
                                move |_, cx| {
                                    hub_rev.update(cx, |hub, cx| {
                                        hub.revoke_secret(rev_id.clone(), cx);
                                    });
                                },
                            ))
                    } else {
                        div().child(pill_button(
                            SharedString::from(format!("conn-rev-arm-{id}")),
                            "キーを削除",
                            Some(Icon::Trash),
                            Tone::Danger,
                            live,
                            theme,
                            move |_, cx| {
                                studio_rev_arm.update(cx, |this, cx| {
                                    this.revoke_armed_for = Some(arm_id.clone());
                                    cx.notify();
                                });
                            },
                        ))
                    },
                ))
            });

        // Kind-specific fields + options_schema editor
        let name_d = self.draft(
            cx,
            format!("conn:{id}:r{rev}:name"),
            "接続名",
            Some(&conn.name),
            theme,
        );
        let cfg = conn.config.as_option().and_then(|c| c.config.as_ref());
        let endpoint = extract_endpoint(cfg);
        let agent_platform = kind_of(conn) == w::ConnectionKind::Llm
            && endpoint
                .as_ref()
                .is_some_and(|ep| ep.adapter == super::GOOGLE_AGENT_PLATFORM);

        let ep_d = self.draft(
            cx,
            format!("conn:{id}:r{rev}:ep"),
            "https://...",
            endpoint.as_ref().map(|e| e.endpoint.as_str()),
            theme,
        );
        let list_init = extract_kind_list_init(cfg);
        let list_label = match kind_of(conn) {
            w::ConnectionKind::Llm => "使用するモデルID（カンマ区切り・空欄で自動）",
            w::ConnectionKind::Tts => "話者IDリスト（カンマ区切り）",
            w::ConnectionKind::Stt => "言語ヒント（例: ja, en）",
            w::ConnectionKind::Embedding => "埋め込みモデルID",
            w::ConnectionKind::Mcp => "許可ツール（カンマ区切り・空欄で全件）",
            w::ConnectionKind::Acp => "起動引数（スペース区切り）",
            w::ConnectionKind::Search => "最大検索結果数 (1–20)",
            w::ConnectionKind::Unspecified => "オプション",
        };
        let list_d = self.draft(
            cx,
            format!("conn:{id}:r{rev}:list"),
            "",
            Some(&list_init),
            theme,
        );
        let cmd_init = match cfg {
            Some(w::connection_config::Config::Mcp(m)) => m.command.as_str(),
            Some(w::connection_config::Config::Acp(a)) => a.command.as_str(),
            _ => "",
        };
        let is_process_kind = matches!(
            kind_of(conn),
            w::ConnectionKind::Mcp | w::ConnectionKind::Acp
        );
        let cmd_d = if is_process_kind {
            Some(self.draft(
                cx,
                format!("conn:{id}:r{rev}:cmd"),
                "実行コマンド",
                Some(cmd_init),
                theme,
            ))
        } else {
            None
        };
        let mcp_args_d = if let Some(w::connection_config::Config::Mcp(mcp)) = cfg {
            let initial = serde_json::json!(mcp.args).to_string();
            Some(self.description_draft(
                cx,
                format!("conn:{id}:r{rev}:mcp-args"),
                r#"["-y", "@scope/mcp-server", "--stdio"]"#,
                Some(&initial),
                theme,
            ))
        } else {
            None
        };
        let mcp_extra_d = if let Some(w::connection_config::Config::Mcp(mcp)) = cfg {
            let env_init = format_kv_lines(&mcp.env, '=');
            let headers_init = format_kv_lines(&mcp.headers, ':');
            Some((
                self.draft(
                    cx,
                    format!("conn:{id}:r{rev}:mcp-cwd"),
                    "作業ディレクトリ (cwd)",
                    Some(&mcp.cwd),
                    theme,
                ),
                self.draft(
                    cx,
                    format!("conn:{id}:r{rev}:mcp-secret-env"),
                    "例: GITHUB_TOKEN",
                    Some(&mcp.secret_env_var),
                    theme,
                ),
                self.draft(
                    cx,
                    format!("conn:{id}:r{rev}:mcp-auth-header"),
                    "例: Authorization / X-Api-Key",
                    Some(&mcp.auth_header),
                    theme,
                ),
                self.description_draft(
                    cx,
                    format!("conn:{id}:r{rev}:mcp-env"),
                    "KEY=VALUE（1行に1つ）",
                    Some(&env_init),
                    theme,
                ),
                self.description_draft(
                    cx,
                    format!("conn:{id}:r{rev}:mcp-headers"),
                    "Header-Name: value（1行に1つ）",
                    Some(&headers_init),
                    theme,
                ),
            ))
        } else {
            None
        };

        // MCP discovery returns raw tool names in the check's resources.
        let mcp_tools_strip = if kind_of(conn) == w::ConnectionKind::Mcp
            && let Some(caps) = check.map(|c| &c.resources).filter(|c| !c.is_empty())
        {
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:mcp-filter"),
                "ツール名で絞り込み…",
                None,
                theme,
            );
            let filter = filter_d.read(cx).text().trim().to_lowercase();
            let current_allowed = split_comma(list_d.read(cx).text());
            let mut chips = Vec::new();
            for cap in caps {
                if !filter.is_empty() && !cap.to_lowercase().contains(&filter) {
                    continue;
                }
                let selected = current_allowed.iter().any(|a| a == cap);
                let ld = list_d.clone();
                let next_list = toggle_comma_item(&current_allowed, cap);
                chips.push(pill_button(
                    SharedString::from(format!("mcp-tool-{id}-{cap}")),
                    cap.clone(),
                    selected.then_some(Icon::Check),
                    if selected { Tone::Primary } else { Tone::Ghost },
                    true,
                    theme,
                    move |_, cx| {
                        ld.update(cx, |d, cx| d.set(next_list.clone(), cx));
                    },
                ));
            }
            Some(
                div()
                    .rounded(px(10.))
                    .bg(theme.bg_blush)
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child(format!("検出されたMCPツール（{}件）", caps.len())),
                    )
                    .when(caps.len() > 20, |this| this.child(filter_d))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.))
                            .children(chips),
                    ),
            )
        } else {
            None
        };

        let is_checking = self.hub.read(cx).checking_connections.contains(&id);
        let is_voicevox = kind_of(conn) == w::ConnectionKind::Tts
            && !super::is_sakura_conn(conn)
            && endpoint
                .as_ref()
                .is_some_and(|ep| ep.adapter.eq_ignore_ascii_case("voicevox"));
        let is_sakura_tts = kind_of(conn) == w::ConnectionKind::Tts && super::is_sakura_conn(conn);
        let dim_d = (kind_of(conn) == w::ConnectionKind::Embedding).then(|| {
            let init = match cfg {
                Some(w::connection_config::Config::Embedding(emb)) if emb.dimensions > 0 => {
                    emb.dimensions.to_string()
                }
                _ => super::default_embedding_dimensions(&list_init).to_string(),
            };
            self.draft(
                cx,
                format!("conn:{id}:r{rev}:dims"),
                "例: 1024",
                Some(&init),
                theme,
            )
        });
        let next_dim = dim_d.as_ref().map(|d| d.read(cx).text().trim().to_owned());

        let dropdown_picker = if kind_of(conn) == w::ConnectionKind::Llm {
            let model_choices = self.hub.read(cx).available_models_for_connection(&id);
            let dd_key = format!("conn-model:{id}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:model-filter"),
                "モデル名で絞り込み…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let current_selected = split_comma(list_d.read(cx).text()).into_iter().next();
            let options: Vec<DropdownOption> = model_choices
                .iter()
                .map(|(mid, label)| DropdownOption {
                    value: mid.clone(),
                    label: label.clone(),
                    detail: (label != mid).then(|| mid.clone()),
                })
                .collect();
            let selected_label = current_selected.as_deref().map(|sel| {
                options
                    .iter()
                    .find(|o| o.value == sel)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| sel.to_owned())
            });
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_refresh = self.hub.clone();
            let hub_select = self.hub.clone();
            let refresh_id = id.clone();
            let select_conn_id = id.clone();
            let list_d_pick = list_d.clone();
            let toggle_key = dd_key.clone();
            Some(
                div()
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
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink_muted)
                                            .child("使用するモデル"),
                                    )
                                    .when(!options.is_empty(), |this| {
                                        this.child(kind_pill(
                                            format!("{}件の候補", options.len()),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(pill_button(
                                SharedString::from(format!("conn-fetch-models-{id}")),
                                if is_checking {
                                    "取得中…"
                                } else {
                                    "モデル一覧を取得"
                                },
                                Some(Icon::Refresh),
                                Tone::Ghost,
                                live && !is_checking,
                                theme,
                                move |_, cx| {
                                    hub_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(refresh_id.clone(), cx);
                                    });
                                },
                            )),
                    )
                    .child(dropdown_select(
                        dd_key,
                        current_selected.as_deref(),
                        selected_label.as_deref(),
                        "モデルを選択…",
                        &options,
                        dd_open,
                        live,
                        240.,
                        Some(filter_d),
                        &filter_txt,
                        "「モデル一覧を取得」を押して利用可能なモデルを読み込んでください",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
                            hub_select.update(cx, |hub, cx| {
                                hub.select_connection_model(select_conn_id.clone(), picked, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    )),
            )
        } else if is_voicevox {
            let speaker_choices = self.hub.read(cx).voice_choices_for_connection(&id);
            let dd_key = format!("conn-vv-speaker:{id}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:vv-filter"),
                "話者名・IDで絞り込み…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let current_selected = split_comma(list_d.read(cx).text())
                .into_iter()
                .next()
                .or_else(|| {
                    endpoint
                        .as_ref()
                        .and_then(|ep| serde_json::from_str::<serde_json::Value>(&ep.options).ok())
                        .and_then(|v| v.get("speaker").and_then(|s| s.as_u64()))
                        .map(|n| n.to_string())
                });
            let options: Vec<DropdownOption> = speaker_choices
                .iter()
                .map(|(sid, label)| DropdownOption {
                    value: sid.clone(),
                    label: label.clone(),
                    detail: Some(format!("ID: {sid}")),
                })
                .collect();
            let selected_label = current_selected.as_deref().map(|sel| {
                options
                    .iter()
                    .find(|o| o.value == sel)
                    .map(|o| format!("{} (ID: {})", o.label, o.value))
                    .unwrap_or_else(|| sel.to_owned())
            });
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_refresh = self.hub.clone();
            let hub_select = self.hub.clone();
            let refresh_id = id.clone();
            let select_conn_id = id.clone();
            let list_d_pick = list_d.clone();
            let toggle_key = dd_key.clone();
            Some(
                div()
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
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink_muted)
                                            .child("既定の話者"),
                                    )
                                    .when(!options.is_empty(), |this| {
                                        this.child(kind_pill(
                                            format!("{}件取得済み", options.len()),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(pill_button(
                                SharedString::from(format!("conn-fetch-vv-{id}")),
                                if is_checking {
                                    "起動・取得中…"
                                } else {
                                    "話者一覧を取得"
                                },
                                Some(Icon::Refresh),
                                Tone::Ghost,
                                live && !is_checking,
                                theme,
                                move |_, cx| {
                                    hub_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(refresh_id.clone(), cx);
                                    });
                                },
                            )),
                    )
                    .child(dropdown_select(
                        dd_key,
                        current_selected.as_deref(),
                        selected_label.as_deref(),
                        "話者を選択…",
                        &options,
                        dd_open,
                        live,
                        240.,
                        Some(filter_d),
                        &filter_txt,
                        "「話者一覧を取得」を押すとVOICEVOXを起動して話者一覧を読み込みます",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
                            hub_select.update(cx, |hub, cx| {
                                hub.select_voicevox_speaker(select_conn_id.clone(), picked, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    )),
            )
        } else if is_sakura_tts {
            let voice_choices = self.hub.read(cx).voice_choices_for_connection(&id);
            let dd_key = format!("conn-sakura-tts:{id}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:sakura-tts-filter"),
                "話者名・モデルIDで絞り込み…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let current_selected = split_comma(list_d.read(cx).text())
                .into_iter()
                .next()
                .or_else(|| voice_choices.first().map(|(vid, _)| vid.clone()));
            let options: Vec<DropdownOption> = voice_choices
                .iter()
                .map(|(vid, label)| DropdownOption {
                    value: vid.clone(),
                    label: label.clone(),
                    detail: (label != vid).then(|| vid.clone()),
                })
                .collect();
            let selected_label = current_selected.as_deref().map(|sel| {
                options
                    .iter()
                    .find(|o| o.value == sel)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| sel.to_owned())
            });
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_refresh = self.hub.clone();
            let hub_select = self.hub.clone();
            let refresh_id = id.clone();
            let select_conn_id = id.clone();
            let list_d_pick = list_d.clone();
            let toggle_key = dd_key.clone();
            Some(
                div()
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
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink_muted)
                                            .child("話者モデル・スタイル"),
                                    )
                                    .when(!options.is_empty(), |this| {
                                        this.child(kind_pill(
                                            format!("{}件利用可能", options.len()),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(pill_button(
                                SharedString::from(format!("conn-fetch-sakura-tts-{id}")),
                                if is_checking {
                                    "取得中…"
                                } else {
                                    "話者一覧を取得"
                                },
                                Some(Icon::Refresh),
                                Tone::Ghost,
                                live && !is_checking,
                                theme,
                                move |_, cx| {
                                    hub_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(refresh_id.clone(), cx);
                                    });
                                },
                            )),
                    )
                    .child(dropdown_select(
                        dd_key,
                        current_selected.as_deref(),
                        selected_label.as_deref(),
                        "話者を選択…",
                        &options,
                        dd_open,
                        live,
                        240.,
                        Some(filter_d),
                        &filter_txt,
                        "話者候補がありません",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
                            hub_select.update(cx, |hub, cx| {
                                hub.select_tts_voice(select_conn_id.clone(), picked, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("※ さくらのクラウドのコントロールパネルで各音声モデルの利用規約に同意してください"),
                    ),
            )
        } else if kind_of(conn) == w::ConnectionKind::Stt {
            let stt_choices = self.hub.read(cx).stt_model_choices_for_connection(&id);
            let dd_key = format!("conn-stt-model:{id}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:stt-filter"),
                "文字起こしモデルを検索…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let configured_stt_model = endpoint
                .as_ref()
                .and_then(|ep| serde_json::from_str::<serde_json::Value>(&ep.options).ok())
                .and_then(|v| {
                    v.get("model")
                        .and_then(|m| m.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                })
                .or_else(|| stt_choices.first().map(|(mid, _)| mid.clone()));
            let options: Vec<DropdownOption> = stt_choices
                .iter()
                .map(|(mid, label)| DropdownOption {
                    value: mid.clone(),
                    label: label.clone(),
                    detail: (label != mid).then(|| mid.clone()),
                })
                .collect();
            let selected_label = configured_stt_model.as_deref().map(str::to_owned);
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_refresh = self.hub.clone();
            let hub_select = self.hub.clone();
            let refresh_id = id.clone();
            let select_conn_id = id.clone();
            let toggle_key = dd_key.clone();
            Some(
                div()
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
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink_muted)
                                            .child("音声認識モデル"),
                                    )
                                    .when(!options.is_empty(), |this| {
                                        this.child(kind_pill(
                                            format!("{}件取得済み", options.len()),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(pill_button(
                                SharedString::from(format!("conn-fetch-stt-{id}")),
                                if is_checking {
                                    "取得中…"
                                } else {
                                    "モデル一覧を取得"
                                },
                                Some(Icon::Refresh),
                                Tone::Ghost,
                                live && !is_checking,
                                theme,
                                move |_, cx| {
                                    hub_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(refresh_id.clone(), cx);
                                    });
                                },
                            )),
                    )
                    .child(dropdown_select(
                        dd_key,
                        configured_stt_model.as_deref(),
                        selected_label.as_deref(),
                        "文字起こしモデルを選択…",
                        &options,
                        dd_open,
                        live,
                        200.,
                        Some(filter_d),
                        &filter_txt,
                        "利用可能なSTTモデルがありません",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            hub_select.update(cx, |hub, cx| {
                                hub.select_stt_model(select_conn_id.clone(), picked, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    )),
            )
        } else if kind_of(conn) == w::ConnectionKind::Embedding {
            let emb_choices = self
                .hub
                .read(cx)
                .embedding_model_choices_for_connection(&id);
            let dd_key = format!("conn-emb-model:{id}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("conn:{id}:emb-filter"),
                "埋め込みモデルを検索…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let current_raw = list_d.read(cx).text().trim().to_owned();
            let current_selected = (!current_raw.is_empty())
                .then_some(current_raw)
                .or_else(|| emb_choices.first().map(|(mid, _)| mid.clone()));
            let options: Vec<DropdownOption> = emb_choices
                .iter()
                .map(|(mid, label)| DropdownOption {
                    value: mid.clone(),
                    label: label.clone(),
                    detail: Some(format!(
                        "推奨次元: {}",
                        super::default_embedding_dimensions(mid)
                    )),
                })
                .collect();
            let selected_label = current_selected.as_deref().map(str::to_owned);
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_refresh = self.hub.clone();
            let hub_select = self.hub.clone();
            let refresh_id = id.clone();
            let select_conn_id = id.clone();
            let list_d_pick = list_d.clone();
            let dim_d_pick = dim_d.clone();
            let toggle_key = dd_key.clone();
            Some(
                div()
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
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink_muted)
                                            .child("埋め込みモデル"),
                                    )
                                    .when(!options.is_empty(), |this| {
                                        this.child(kind_pill(
                                            format!("{}件利用可能", options.len()),
                                            theme,
                                        ))
                                    }),
                            )
                            .child(pill_button(
                                SharedString::from(format!("conn-fetch-emb-{id}")),
                                if is_checking {
                                    "確認・取得中…"
                                } else {
                                    "接続確認・取得"
                                },
                                Some(Icon::Refresh),
                                Tone::Ghost,
                                live && !is_checking,
                                theme,
                                move |_, cx| {
                                    hub_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(refresh_id.clone(), cx);
                                    });
                                },
                            )),
                    )
                    .child(dropdown_select(
                        dd_key,
                        current_selected.as_deref(),
                        selected_label.as_deref(),
                        "埋め込みモデルを選択…",
                        &options,
                        dd_open,
                        live,
                        200.,
                        Some(filter_d),
                        &filter_txt,
                        "利用可能な埋め込みモデルがありません",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            list_d_pick.update(cx, |d, cx| d.set(picked.clone(), cx));
                            if let Some(ref dim_draft) = dim_d_pick {
                                let dims = super::default_embedding_dimensions(&picked);
                                dim_draft.update(cx, |d, cx| d.set(dims.to_string(), cx));
                            }
                            hub_select.update(cx, |hub, cx| {
                                hub.select_embedding_model(select_conn_id.clone(), picked, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    )),
            )
        } else {
            None
        };

        // Dynamic options_schema fields
        let (schema_controls, next_options_json) = if agent_platform {
            let options = endpoint
                .as_ref()
                .and_then(|ep| serde_json::from_str::<serde_json::Value>(&ep.options).ok())
                .unwrap_or_default();
            let project = self.draft(
                cx,
                format!("conn:{id}:r{rev}:project_id"),
                "Express mode では空欄",
                options["project_id"].as_str(),
                theme,
            );
            let location = self.draft(
                cx,
                format!("conn:{id}:r{rev}:location"),
                "未指定時は global",
                options["location"].as_str(),
                theme,
            );
            let options: serde_json::Map<String, serde_json::Value> = [
                ("project_id", project.read(cx).text().trim()),
                ("location", location.read(cx).text().trim()),
            ]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| (key.to_owned(), value.into()))
            .collect();
            (
                vec![
                    labeled_field("Google Cloud プロジェクト ID", project, theme)
                        .into_any_element(),
                    labeled_field("ロケーション", location, theme).into_any_element(),
                ],
                Some(serde_json::Value::Object(options).to_string()),
            )
        } else {
            self.render_options_schema(&id, rev, endpoint.as_ref(), theme, cx)
        };

        let next_name = name_d.read(cx).text().trim().to_owned();
        let next_ep = ep_d.read(cx).text().trim().to_owned();
        let next_list = list_d.read(cx).text().trim().to_owned();
        let next_cmd = cmd_d.as_ref().map(|d| d.read(cx).text().trim().to_owned());
        let next_args = mcp_args_d.as_ref().map_or(Ok(Vec::new()), |draft| {
            parse_process_args(draft.read(cx).text())
        });
        let args_error = next_args.as_ref().err().copied();
        let next_mcp_extra =
            mcp_extra_d
                .as_ref()
                .map(|(cwd_d, secret_env_d, auth_hdr_d, env_d, headers_d)| {
                    let env_parsed = parse_kv_lines(env_d.read(cx).text(), '=');
                    let headers_parsed = parse_kv_lines(headers_d.read(cx).text(), ':');
                    (
                        cwd_d.read(cx).text().trim().to_owned(),
                        secret_env_d.read(cx).text().trim().to_owned(),
                        auth_hdr_d.read(cx).text().trim().to_owned(),
                        env_parsed,
                        headers_parsed,
                    )
                });
        let mcp_kv_error = next_mcp_extra.as_ref().and_then(|(_, _, _, env_r, hdr_r)| {
            env_r
                .as_ref()
                .err()
                .or_else(|| hdr_r.as_ref().err())
                .copied()
        });

        let conn_for_save = conn.clone();
        let hub_save = self.hub.clone();

        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child("接続を有効にする"),
                    )
                    .child(toggle_switch(
                        SharedString::from(format!("conn-enable-{id}")),
                        conn.enabled,
                        live,
                        theme,
                        move |_, cx| {
                            hub_toggle.update(cx, |hub, cx| {
                                hub.set_connection_enabled(
                                    conn_toggle.clone(),
                                    !conn_toggle.enabled,
                                    cx,
                                );
                            });
                        },
                    )),
            )
            .when_some(err_strip, |this, strip| this.child(strip))
            .child(cred_strip)
            .child(labeled_field("表示名", name_d, theme))
            .when_some(cmd_d, |this, cd| {
                this.child(labeled_field("コマンド", cd, theme))
            })
            .when_some(mcp_args_d, |this, draft| {
                this.child(labeled_field(
                    "起動引数（JSON 配列または空白区切り）",
                    div()
                        .id(SharedString::from(format!("conn-mcp-args-{id}")))
                        .max_h(px(124.))
                        .overflow_y_scroll()
                        .child(draft),
                    theme,
                ))
                .when_some(args_error, |this, error| {
                    this.child(div().text_xs().text_color(theme.danger_fg).child(error))
                })
            })
            .when_some(mcp_extra_d, |this, (cwd_d, secret_env_d, auth_hdr_d, env_d, headers_d)| {
                this.child(labeled_field("作業ディレクトリ (cwd)", cwd_d, theme))
                    .child(labeled_field("認証シークレット環境変数名 (stdio)", secret_env_d, theme))
                    .child(labeled_field("認証ヘッダー名 (HTTP・空欄時は Bearer)", auth_hdr_d, theme))
                    .child(labeled_field(
                        "環境変数（KEY=VALUE を1行ずつ）",
                        div()
                            .id(SharedString::from(format!("conn-mcp-env-{id}")))
                            .max_h(px(110.))
                            .overflow_y_scroll()
                            .child(env_d),
                        theme,
                    ))
                    .child(labeled_field(
                        "HTTP ヘッダー（Name: Value を1行ずつ）",
                        div()
                            .id(SharedString::from(format!("conn-mcp-headers-{id}")))
                            .max_h(px(110.))
                            .overflow_y_scroll()
                            .child(headers_d),
                        theme,
                    ))
                    .when_some(mcp_kv_error, |this, error| {
                        this.child(div().text_xs().text_color(theme.danger_fg).child(error))
                    })
            })
            .when(!agent_platform, |this| {
                this.child(labeled_field("エンドポイント URL", ep_d, theme))
            })
            .when_some(dropdown_picker, |this, picker| this.child(picker))
            .when_some(dim_d, |this, dd| {
                this.child(labeled_field("次元数 (dimensions)", dd, theme))
            })
            .when(
                agent_platform
                    || (!is_voicevox
                        && !is_sakura_tts
                        && !matches!(
                            kind_of(conn),
                            w::ConnectionKind::Llm | w::ConnectionKind::Embedding
                        )),
                |this| this.child(labeled_field(list_label, list_d, theme)),
            )
            .when_some(mcp_tools_strip, |this, strip| this.child(strip))
            .when(!schema_controls.is_empty(), |this| {
                this.child(
                    div()
                        .rounded(px(10.))
                        .bg(theme.bg_blush)
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .children(schema_controls),
                )
            })
            .child(div().flex().flex_row().justify_between().gap(px(8.)).child(pill_button(
                SharedString::from(format!("conn-delete-{id}")),
                "接続を削除",
                Some(Icon::Trash),
                Tone::Danger,
                live,
                theme,
                move |window, cx| {
                    let hub = hub_delete.clone();
                    let id = delete_id.clone();
                    let answer = window.prompt(
                        gpui::PromptLevel::Warning,
                        &delete_title,
                        Some("この接続の設定、保存したキー、コンパニオンへのモデル割り当てを削除します。"),
                        &["キャンセル", "削除"],
                        cx,
                    );
                    cx.spawn(async move |cx| {
                        if answer.await == Ok(1) {
                            let _ = cx.update(|cx| {
                                hub.update(cx, |hub, cx| hub.delete_connection(id, rev, cx));
                            });
                        }
                    }).detach();
                },
            )).child(pill_button(
                SharedString::from(format!("conn-save-{id}")),
                "保存",
                Some(Icon::Check),
                Tone::Primary,
                live && !next_name.is_empty() && args_error.is_none() && mcp_kv_error.is_none(),
                theme,
                move |_, cx| {
                    let Ok(args) = &next_args else { return };
                    let mut input = apply_connection_edits(
                        &conn_for_save,
                        &next_name,
                        &next_ep,
                        next_cmd.as_deref().map(|command| (command, args.as_slice())),
                        &next_list,
                        next_dim.as_deref(),
                        next_options_json.as_deref(),
                    );
                    if let Some((cwd, secret_env, auth_hdr, Ok(env), Ok(headers))) =
                        &next_mcp_extra
                    {
                        apply_mcp_extra_edits(
                            &mut input,
                            cwd,
                            secret_env,
                            auth_hdr,
                            env,
                            headers,
                        );
                    }
                    hub_save.update(cx, |hub, cx| {
                        hub.upsert_connection("接続設定の保存", input, cx);
                    });
                },
            )))
            .into_any_element()
    }

    fn render_options_schema(
        &mut self,
        id: &str,
        rev: u64,
        endpoint: Option<&w::ConnectionEndpoint>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Option<String>) {
        let Some(ep) = endpoint else {
            return (Vec::new(), None);
        };
        if ep.options_schema.trim().is_empty() {
            return (Vec::new(), None);
        }
        let Ok(serde_json::Value::Object(schema_obj)) =
            serde_json::from_str::<serde_json::Value>(&ep.options_schema)
        else {
            return (Vec::new(), None);
        };
        let Some(serde_json::Value::Object(props)) = schema_obj.get("properties") else {
            return (Vec::new(), None);
        };

        let mut opts = match serde_json::from_str::<serde_json::Value>(&ep.options) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };

        let mut controls: Vec<AnyElement> = Vec::new();
        for (key, prop) in props {
            let prop_type = prop
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("string");
            let current_val = opts.get(key).cloned();

            if let Some(serde_json::Value::Array(choices)) = prop.get("enum") {
                let d_key = format!("conn:{id}:r{rev}:opt:{key}");
                let init = current_val
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_default();
                let d = self.draft(cx, d_key, "", Some(&init), theme);
                let active_val = d.read(cx).text().to_owned();
                if !active_val.is_empty() {
                    opts.insert(key.clone(), serde_json::Value::String(active_val.clone()));
                }
                let mut chips = Vec::new();
                for choice in choices {
                    if let Some(s) = choice.as_str() {
                        let picked = active_val == s;
                        let target_d = d.clone();
                        let val_str = s.to_owned();
                        chips.push(pill_button(
                            SharedString::from(format!("opt-enum-{id}-{key}-{s}")),
                            s.to_owned(),
                            picked.then_some(Icon::Check),
                            if picked { Tone::Primary } else { Tone::Ghost },
                            true,
                            theme,
                            move |_, cx| {
                                target_d.update(cx, |draft, cx| draft.set(val_str.clone(), cx));
                            },
                        ));
                    }
                }
                controls.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink_muted)
                                .child(key.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(6.))
                                .children(chips),
                        )
                        .into_any_element(),
                );
            } else if prop_type == "boolean" {
                let d_key = format!("conn:{id}:r{rev}:opt:{key}");
                let init = current_val
                    .as_ref()
                    .and_then(|v| v.as_bool())
                    .map(|b| if b { "true" } else { "false" })
                    .unwrap_or("false");
                let d = self.draft(cx, d_key, "", Some(init), theme);
                let checked = d.read(cx).text() == "true";
                opts.insert(key.clone(), serde_json::Value::Bool(checked));
                let target_d = d.clone();
                controls.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink)
                                .child(key.clone()),
                        )
                        .child(toggle_switch(
                            SharedString::from(format!("opt-bool-{id}-{key}")),
                            checked,
                            true,
                            theme,
                            move |_, cx| {
                                target_d.update(cx, |draft, cx| {
                                    draft.set(if checked { "false" } else { "true" }, cx);
                                });
                            },
                        ))
                        .into_any_element(),
                );
            } else {
                let init = current_val.as_ref().map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                let d = self.draft(
                    cx,
                    format!("conn:{id}:r{rev}:opt:{key}"),
                    "",
                    init.as_deref(),
                    theme,
                );
                let txt = d.read(cx).text().trim().to_owned();
                if !txt.is_empty() {
                    let parsed = match prop_type {
                        "integer" => txt
                            .parse::<i64>()
                            .map(|n| serde_json::Value::Number(n.into()))
                            .unwrap_or_else(|_| serde_json::Value::String(txt.clone())),
                        "number" => txt
                            .parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or_else(|| serde_json::Value::String(txt.clone())),
                        _ => serde_json::Value::String(txt.clone()),
                    };
                    opts.insert(key.clone(), parsed);
                }
                controls.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink_muted)
                                .child(key.clone()),
                        )
                        .child(d)
                        .into_any_element(),
                );
            }
        }

        let serialized = (!opts.is_empty()).then(|| serde_json::Value::Object(opts).to_string());
        (controls, serialized)
    }

    fn render_advanced_roles_panel(
        &mut self,
        companion: Option<&w::Companion>,
        all_models: &[(String, String, String)],
        embedding_models: &[(String, String, String)],
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(comp) = companion else {
            return div()
                .w_full()
                .bg(theme.bg_surface)
                .border_1()
                .border_color(theme.border)
                .rounded(px(14.))
                .p(px(16.))
                .text_xs()
                .text_color(theme.ink_muted)
                .child("コンパニオンを選択すると、役割別のモデル割当を調整できます。")
                .into_any_element();
        };
        let assignments = comp.assignments.as_option().cloned().unwrap_or_default();
        let (refresh_ids, checking_any, stt_models, active_stt, tts_voices, active_tts, conn_names) = {
            let hub = self.hub.read(cx);
            let ids: Vec<String> = hub
                .model
                .settings()
                .map(|s| {
                    s.connections
                        .iter()
                        .filter(|c| {
                            c.enabled
                                && matches!(
                                    kind_of(c),
                                    w::ConnectionKind::Llm
                                        | w::ConnectionKind::Embedding
                                        | w::ConnectionKind::Stt
                                        | w::ConnectionKind::Tts
                                )
                        })
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            let names: std::collections::BTreeMap<String, String> = hub
                .model
                .settings()
                .map(|s| {
                    s.connections
                        .iter()
                        .map(|c| (c.id.clone(), c.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (
                ids,
                !hub.checking_connections.is_empty(),
                hub.all_stt_models(),
                hub.active_stt_selection(),
                hub.all_tts_voices(),
                hub.active_tts_selection(),
                names,
            )
        };
        let hub_refresh_all = self.hub.clone();
        let refresh_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("役割ごとに使用するモデルをドロップダウンから割り当てます。"),
            )
            .when(!refresh_ids.is_empty(), |this| {
                let ids = refresh_ids.clone();
                this.child(pill_button(
                    "adv-roles-refresh-all",
                    if checking_any {
                        "モデル一覧を取得中…"
                    } else {
                        "全接続のモデルを再取得"
                    },
                    Some(Icon::Refresh),
                    Tone::Ghost,
                    live && !checking_any,
                    theme,
                    move |_, cx| {
                        hub_refresh_all.update(cx, |hub, cx| {
                            for cid in &ids {
                                hub.check_connection(cid.clone(), cx);
                            }
                        });
                    },
                ))
            });

        let mut rows = Vec::new();
        for (idx, slot) in ModelRoleSlot::ALL.into_iter().enumerate() {
            let slot_models = if slot == ModelRoleSlot::Embedding {
                embedding_models
            } else {
                all_models
            };
            let current = slot.current(&assignments);
            let current_text = current
                .as_ref()
                .map(|m| {
                    slot_models
                        .iter()
                        .find(|(cid, mid, _)| cid == &m.connection_id && mid == &m.model_id)
                        .map(|(_, _, label)| label.clone())
                        .unwrap_or_else(|| format!("{} / {}", m.connection_id, m.model_id))
                })
                .unwrap_or_else(|| slot.fallback_hint().to_owned());

            let dd_key = format!("adv-role:{slot:?}");
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
            let filter_d = self.draft(
                cx,
                format!("adv-role:{slot:?}:filter"),
                "モデル名で絞り込み…",
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let mut options = Vec::new();
            if !matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task) {
                options.push(DropdownOption {
                    value: "__auto__".to_owned(),
                    label: slot.fallback_hint().to_owned(),
                    detail: None,
                });
            }
            for (cid, mid, label) in slot_models {
                options.push(DropdownOption {
                    value: encode_model_choice(cid, mid),
                    label: label.clone(),
                    detail: Some(mid.clone()),
                });
            }
            let current_value = current
                .as_ref()
                .map(|m| encode_model_choice(&m.connection_id, &m.model_id))
                .or_else(|| {
                    (!matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task))
                        .then(|| "__auto__".to_owned())
                });
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let hub_assign = self.hub.clone();
            let toggle_key = dd_key.clone();

            rows.push(
                div()
                    .when(idx > 0, |this| {
                        this.border_t_1().border_color(theme.border).pt(px(12.))
                    })
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
                                    .text_color(theme.ink)
                                    .child(slot.label()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(current_text.clone()),
                            ),
                    )
                    .child(dropdown_select(
                        dd_key,
                        current_value.as_deref(),
                        Some(current_text.as_str()),
                        "モデルを選択…",
                        &options,
                        dd_open,
                        live,
                        200.,
                        Some(filter_d),
                        &filter_txt,
                        "利用可能なモデルがありません（接続を追加・確認してください）",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            if picked == "__auto__" {
                                hub_assign.update(cx, |hub, cx| hub.assign_model(slot, None, cx));
                            } else if let Some((cid, mid)) = decode_model_choice(&picked) {
                                hub_assign.update(cx, |hub, cx| {
                                    hub.assign_model(slot, Some((cid, mid)), cx);
                                });
                            }
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    ))
                    .into_any_element(),
            );
        }

        type VoiceAssignFn =
            fn(&mut super::Hub, Option<(String, String)>, &mut Context<super::Hub>);

        struct VoiceRoleSpec {
            title: &'static str,
            dd_key: &'static str,
            filter_key: &'static str,
            filter_placeholder: &'static str,
            pick_placeholder: &'static str,
            off_label: &'static str,
            empty_dropdown_label: &'static str,
            empty_hint: &'static str,
            add_btn_id: &'static str,
            choices: Vec<(String, String, String)>,
            active: Option<(String, String)>,
            assign: VoiceAssignFn,
        }

        let voice_specs = [
            VoiceRoleSpec {
                title: "音声認識（STT）",
                dd_key: "adv-role:stt",
                filter_key: "adv-role:stt:filter",
                filter_placeholder: "モデル名で絞り込み…",
                pick_placeholder: "モデルを選択…",
                off_label: "無効（音声入力オフ）",
                empty_dropdown_label: "利用可能な音声認識モデルがありません（「接続を追加」から追加してください）",
                empty_hint: "音声認識（STT）接続がありません。「接続を追加」から追加してください。",
                add_btn_id: "adv-role-stt-add",
                choices: stt_models,
                active: active_stt,
                assign: super::Hub::assign_stt_model,
            },
            VoiceRoleSpec {
                title: "音声合成（TTS）",
                dd_key: "adv-role:tts",
                filter_key: "adv-role:tts:filter",
                filter_placeholder: "話者名・接続名で絞り込み…",
                pick_placeholder: "話者を選択…",
                off_label: "無効（読み上げオフ）",
                empty_dropdown_label: "利用可能な音声合成話者がありません（「接続を追加」から追加してください）",
                empty_hint: "音声合成（TTS）接続がありません。「接続を追加」から追加してください。",
                add_btn_id: "adv-role-tts-add",
                choices: tts_voices,
                active: active_tts,
                assign: super::Hub::assign_tts_voice,
            },
        ];

        for spec in voice_specs {
            let current_text = spec
                .active
                .as_ref()
                .map(|(cid, vid)| {
                    spec.choices
                        .iter()
                        .find(|(c, v, _)| c == cid && v == vid)
                        .map(|(_, _, label)| label.clone())
                        .unwrap_or_else(|| {
                            let cname = conn_names
                                .get(cid)
                                .map(String::as_str)
                                .unwrap_or(cid.as_str());
                            format!("{cname} / {vid}")
                        })
                })
                .unwrap_or_else(|| spec.off_label.to_owned());

            let dd_open = self.open_dropdown.as_deref() == Some(spec.dd_key);
            let filter_d = self.draft(
                cx,
                spec.filter_key.to_owned(),
                spec.filter_placeholder,
                None,
                theme,
            );
            let filter_txt = filter_d.read(cx).text().to_owned();
            let mut options = Vec::with_capacity(spec.choices.len() + 1);
            options.push(DropdownOption {
                value: "__off__".to_owned(),
                label: spec.off_label.to_owned(),
                detail: None,
            });
            for (cid, vid, label) in &spec.choices {
                options.push(DropdownOption {
                    value: encode_model_choice(cid, vid),
                    label: label.clone(),
                    detail: Some(vid.clone()),
                });
            }
            let current_value = spec
                .active
                .as_ref()
                .map(|(cid, vid)| encode_model_choice(cid, vid))
                .unwrap_or_else(|| "__off__".to_owned());
            let no_choices = spec.choices.is_empty();
            let assign_fn = spec.assign;
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let studio_add = cx.entity();
            let hub_assign = self.hub.clone();
            let toggle_key = spec.dd_key.to_owned();

            rows.push(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt(px(12.))
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
                                    .text_color(theme.ink)
                                    .child(spec.title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(current_text.clone()),
                            ),
                    )
                    .child(dropdown_select(
                        spec.dd_key,
                        Some(current_value.as_str()),
                        Some(current_text.as_str()),
                        spec.pick_placeholder,
                        &options,
                        dd_open,
                        live,
                        200.,
                        Some(filter_d),
                        &filter_txt,
                        spec.empty_dropdown_label,
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
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
                            if picked == "__off__" {
                                hub_assign.update(cx, |hub, cx| assign_fn(hub, None, cx));
                            } else if let Some((cid, vid)) = decode_model_choice(&picked) {
                                hub_assign
                                    .update(cx, |hub, cx| assign_fn(hub, Some((cid, vid)), cx));
                            }
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    ))
                    .when(no_choices, |this| {
                        this.child(
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
                                        .child(spec.empty_hint),
                                )
                                .child(pill_button(
                                    spec.add_btn_id,
                                    "接続を追加を開く",
                                    Some(Icon::Plus),
                                    Tone::Ghost,
                                    true,
                                    theme,
                                    move |_, cx| {
                                        studio_add.update(cx, |this, cx| {
                                            this.adding_connection = true;
                                            cx.notify();
                                        });
                                    },
                                )),
                        )
                    })
                    .into_any_element(),
            );
        }

        // Web Search role assignment (F13)
        let (search_conns, active_search_id) = {
            let hub = self.hub.read(cx);
            let list: Vec<(String, String, String)> = hub
                .model
                .settings()
                .map(|s| {
                    s.connections
                        .iter()
                        .filter(|c| c.kind == w::ConnectionKind::Search)
                        .map(|c| {
                            let adapter = c
                                .config
                                .as_option()
                                .and_then(|cfg| match cfg.config.as_ref()? {
                                    w::connection_config::Config::Search(s) => {
                                        s.endpoint.as_option().map(|e| e.adapter.clone())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_else(|| "search".into());
                            (c.id.clone(), c.name.clone(), adapter)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let active = hub.model.settings().and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.kind == w::ConnectionKind::Search && c.enabled)
                    .map(|c| c.id.clone())
            });
            (list, active)
        };
        {
            let dd_key = "adv-role:search";
            let dd_open = self.open_dropdown.as_deref() == Some(dd_key);
            let mut options = Vec::with_capacity(search_conns.len() + 1);
            options.push(DropdownOption {
                value: "__off__".to_owned(),
                label: "無効（Web 検索オフ）".to_owned(),
                detail: None,
            });
            for (cid, name, adapter) in &search_conns {
                options.push(DropdownOption {
                    value: cid.clone(),
                    label: name.clone(),
                    detail: Some(adapter.clone()),
                });
            }
            let current_value = active_search_id
                .clone()
                .unwrap_or_else(|| "__off__".to_owned());
            let current_text = active_search_id
                .as_ref()
                .and_then(|aid| {
                    search_conns
                        .iter()
                        .find(|(cid, _, _)| cid == aid)
                        .map(|(_, name, adapter)| format!("{name} ({adapter})"))
                })
                .unwrap_or_else(|| "無効（Web 検索オフ）".to_owned());
            let no_search = search_conns.is_empty();
            let studio_toggle = cx.entity();
            let studio_pick = cx.entity();
            let studio_add = cx.entity();
            let hub_assign = self.hub.clone();
            rows.push(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt(px(12.))
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
                                    .text_color(theme.ink)
                                    .child("Web 検索"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(current_text.clone()),
                            ),
                    )
                    .child(dropdown_select(
                        dd_key,
                        Some(current_value.as_str()),
                        Some(current_text.as_str()),
                        "検索プロバイダを選択…",
                        &options,
                        dd_open,
                        live,
                        180.,
                        None,
                        "",
                        "Web 検索接続がありません（「接続を追加」から追加してください）",
                        theme,
                        move |_, cx| {
                            studio_toggle.update(cx, |this, cx| {
                                this.open_dropdown =
                                    if this.open_dropdown.as_deref() == Some(dd_key) {
                                        None
                                    } else {
                                        Some(dd_key.to_owned())
                                    };
                                cx.notify();
                            });
                        },
                        move |picked, _, cx| {
                            let target = if picked == "__off__" {
                                None
                            } else {
                                Some(picked)
                            };
                            hub_assign.update(cx, |hub, cx| {
                                hub.assign_search_connection(target, cx);
                            });
                            studio_pick.update(cx, |this, cx| {
                                this.open_dropdown = None;
                                cx.notify();
                            });
                        },
                    ))
                    .when(no_search, |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap(px(8.))
                                .child(div().text_xs().text_color(theme.ink_muted).child(
                                    "Web 検索接続がありません。未設定の間は外部検索を行いません。",
                                ))
                                .child(pill_button(
                                    "adv-role-search-add",
                                    "接続を追加を開く",
                                    Some(Icon::Plus),
                                    Tone::Ghost,
                                    true,
                                    theme,
                                    move |_, cx| {
                                        studio_add.update(cx, |this, cx| {
                                            this.adding_connection = true;
                                            cx.notify();
                                        });
                                    },
                                )),
                        )
                    })
                    .into_any_element(),
            );
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
            .child(refresh_bar)
            .children(rows)
            .into_any_element()
    }
}

fn encode_model_choice(connection_id: &str, model_id: &str) -> String {
    format!("{connection_id}\u{1f}{model_id}")
}

fn decode_model_choice(encoded: &str) -> Option<(String, String)> {
    let (cid, mid) = encoded.split_once('\u{1f}')?;
    if cid.is_empty() || mid.is_empty() {
        return None;
    }
    Some((cid.to_owned(), mid.to_owned()))
}

fn kind_label(kind: w::ConnectionKind) -> &'static str {
    match kind {
        w::ConnectionKind::Llm => "LLM",
        w::ConnectionKind::Tts => "音声合成",
        w::ConnectionKind::Stt => "音声認識",
        w::ConnectionKind::Embedding => "埋め込み",
        w::ConnectionKind::Mcp => "MCP",
        w::ConnectionKind::Acp => "ACP",
        w::ConnectionKind::Search => "Web検索",
        w::ConnectionKind::Unspecified => "その他",
    }
}

fn kind_of(conn: &w::Connection) -> w::ConnectionKind {
    conn.kind
        .as_known()
        .unwrap_or(w::ConnectionKind::Unspecified)
}

fn extract_endpoint(cfg: Option<&w::connection_config::Config>) -> Option<w::ConnectionEndpoint> {
    match cfg? {
        w::connection_config::Config::Llm(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Tts(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Stt(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Embedding(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Mcp(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Acp(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Search(c) => c.endpoint.as_option().cloned(),
    }
}

fn extract_kind_list_init(cfg: Option<&w::connection_config::Config>) -> String {
    match cfg {
        Some(w::connection_config::Config::Llm(c)) => c.model_ids.join(", "),
        Some(w::connection_config::Config::Tts(c)) => c.voice_ids.join(", "),
        Some(w::connection_config::Config::Stt(c)) => c.language_hints.join(", "),
        Some(w::connection_config::Config::Embedding(c)) => c.model_id.clone(),
        Some(w::connection_config::Config::Mcp(c)) => c.allowed_tools.join(", "),
        Some(w::connection_config::Config::Acp(c)) => c.args.join(" "),
        Some(w::connection_config::Config::Search(c)) => {
            if c.max_results == 0 {
                "5".into()
            } else {
                c.max_results.to_string()
            }
        }
        None => String::new(),
    }
}

fn format_kv_lines<S: std::hash::BuildHasher>(
    map: &std::collections::HashMap<String, String, S>,
    sep: char,
) -> String {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort_by_key(|(a, _)| *a);
    pairs
        .into_iter()
        .map(|(k, v)| match sep {
            ':' => format!("{k}: {v}"),
            _ => format!("{k}{sep}{v}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_kv_lines(raw: &str, sep: char) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once(sep) else {
            return Err(match sep {
                ':' => "ヘッダーは `Name: Value` 形式で入力してください。",
                _ => "環境変数は `KEY=VALUE` 形式で入力してください。",
            });
        };
        let key = k.trim();
        if key.is_empty() {
            return Err("空のキーは指定できません。");
        }
        out.push((key.to_owned(), v.trim().to_owned()));
    }
    Ok(out)
}

fn apply_mcp_extra_edits(
    input: &mut w::ConnectionInput,
    cwd: &str,
    secret_env_var: &str,
    auth_header: &str,
    env: &[(String, String)],
    headers: &[(String, String)],
) {
    let mut cfg = input.config.as_option().cloned().unwrap_or_default();
    if let Some(w::connection_config::Config::Mcp(mcp)) = cfg.config.as_mut() {
        mcp.cwd = cwd.to_owned();
        mcp.secret_env_var = secret_env_var.to_owned();
        mcp.auth_header = auth_header.to_owned();
        mcp.env = env.iter().cloned().collect();
        mcp.headers = headers.iter().cloned().collect();
    }
    input.config = Some(cfg).into();
}

fn split_comma(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn toggle_comma_item(items: &[String], item: &str) -> String {
    let mut next = items.to_vec();
    if let Some(pos) = next.iter().position(|x| x == item) {
        next.remove(pos);
    } else {
        next.push(item.to_owned());
    }
    next.join(", ")
}

fn apply_connection_edits(
    conn: &w::Connection,
    name: &str,
    endpoint_url: &str,
    process: Option<(&str, &[String])>,
    list_raw: &str,
    dimensions_raw: Option<&str>,
    options_json: Option<&str>,
) -> w::ConnectionInput {
    let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
    let updated_ep = |cur: Option<&w::ConnectionEndpoint>| {
        let mut updated = cur.cloned().unwrap_or_default();
        updated.endpoint = endpoint_url.to_owned();
        if let Some(opts) = options_json {
            updated.options = opts.to_owned();
        }
        Some(updated).into()
    };
    match cfg.config.as_mut() {
        Some(w::connection_config::Config::Llm(llm)) => {
            llm.endpoint = updated_ep(llm.endpoint.as_option());
            llm.model_ids = split_comma(list_raw);
        }
        Some(w::connection_config::Config::Tts(tts)) => {
            let mut ep_val = updated_ep(tts.endpoint.as_option())
                .into_option()
                .unwrap_or_default();
            let voices = split_comma(list_raw);
            let sakura = ep_val.adapter.eq_ignore_ascii_case("sakura")
                || super::is_sakura_endpoint(&ep_val.endpoint);
            if sakura && let Some(first) = voices.first() {
                let mut opts = match serde_json::from_str::<serde_json::Value>(&ep_val.options) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => serde_json::Map::new(),
                };
                if let Some((m, v)) = first.split_once('/') {
                    if !m.trim().is_empty() {
                        opts.insert("model".into(), serde_json::json!(m.trim()));
                    }
                    if !v.trim().is_empty() {
                        opts.insert("voice".into(), serde_json::json!(v.trim()));
                    }
                } else if !first.trim().is_empty() {
                    opts.insert("model".into(), serde_json::json!(first.trim()));
                }
                opts.entry("protocol".to_owned())
                    .or_insert_with(|| serde_json::json!("sakura"));
                opts.entry("response_format".to_owned())
                    .or_insert_with(|| serde_json::json!("wav"));
                ep_val.options = serde_json::Value::Object(opts).to_string();
            } else if ep_val.adapter.eq_ignore_ascii_case("voicevox")
                && let Some(first) = voices.first()
                && let Ok(speaker_num) = first.trim().parse::<u32>()
            {
                let mut opts = match serde_json::from_str::<serde_json::Value>(&ep_val.options) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => serde_json::Map::new(),
                };
                opts.insert("speaker".into(), serde_json::json!(speaker_num));
                ep_val.options = serde_json::Value::Object(opts).to_string();
            }
            tts.endpoint = Some(ep_val).into();
            tts.voice_ids = voices;
        }
        Some(w::connection_config::Config::Stt(stt)) => {
            let mut ep_val = updated_ep(stt.endpoint.as_option())
                .into_option()
                .unwrap_or_default();
            let hints = split_comma(list_raw);
            if let Some(first_lang) = hints.first() {
                let mut opts = match serde_json::from_str::<serde_json::Value>(&ep_val.options) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => serde_json::Map::new(),
                };
                opts.insert("language".into(), serde_json::json!(first_lang));
                ep_val.options = serde_json::Value::Object(opts).to_string();
            }
            stt.endpoint = Some(ep_val).into();
            stt.language_hints = hints;
        }
        Some(w::connection_config::Config::Embedding(emb)) => {
            let mut ep_val = updated_ep(emb.endpoint.as_option())
                .into_option()
                .unwrap_or_default();
            if ep_val.adapter.trim().is_empty() {
                ep_val.adapter = "openai".to_owned();
            }
            let sakura = ep_val.adapter.eq_ignore_ascii_case("sakura")
                || super::is_sakura_endpoint(&ep_val.endpoint);
            emb.endpoint = Some(ep_val).into();
            let trimmed = list_raw.trim();
            emb.model_id = if trimmed.is_empty() {
                if sakura {
                    "multilingual-e5-large".to_owned()
                } else {
                    "text-embedding-3-small".to_owned()
                }
            } else {
                trimmed.to_owned()
            };
            let parsed_dim = dimensions_raw
                .and_then(|raw| raw.trim().parse::<u32>().ok())
                .filter(|&d| d > 0);
            if let Some(dims) = parsed_dim {
                emb.dimensions = dims;
            } else if emb.dimensions == 0 {
                emb.dimensions = super::default_embedding_dimensions(&emb.model_id);
            }
        }
        Some(w::connection_config::Config::Mcp(mcp)) => {
            let mut ep_val = updated_ep(mcp.endpoint.as_option())
                .into_option()
                .unwrap_or_default();
            ep_val.adapter = "mcp".to_owned();
            if let Some((cmd, args)) = process {
                let trimmed = cmd.trim();
                if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    ep_val.endpoint = trimmed.to_owned();
                    mcp.command.clear();
                    mcp.args.clear();
                } else {
                    mcp.command = trimmed.to_owned();
                    mcp.args = args.to_vec();
                }
            }
            mcp.endpoint = Some(ep_val).into();
            mcp.allowed_tools = split_comma(list_raw);
        }
        Some(w::connection_config::Config::Acp(acp)) => {
            acp.endpoint = updated_ep(acp.endpoint.as_option());
            if let Some((cmd, _)) = process {
                acp.command = cmd.to_owned();
            }
            acp.args = list_raw.split_whitespace().map(str::to_owned).collect();
        }
        Some(w::connection_config::Config::Search(search)) => {
            let mut ep_val = updated_ep(search.endpoint.as_option())
                .into_option()
                .unwrap_or_default();
            if ep_val.adapter.trim().is_empty() {
                ep_val.adapter = "mwmbl".to_owned();
            }
            search.endpoint = Some(ep_val).into();
            search.max_results = list_raw
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|&n| (1..=20).contains(&n))
                .unwrap_or(5);
        }
        None => {}
    }

    w::ConnectionInput {
        id: conn.id.clone(),
        name: name.to_owned(),
        enabled: conn.enabled,
        expected_revision: conn.revision,
        kind: conn.kind,
        config: Some(cfg).into(),
        ..Default::default()
    }
}

fn build_custom_connection_input(
    kind: w::ConnectionKind,
    name: &str,
    adapter_or_cmd: &str,
    url_or_args: &str,
) -> Option<w::ConnectionInput> {
    let ep = w::ConnectionEndpoint {
        adapter: adapter_or_cmd.to_owned(),
        endpoint: url_or_args.to_owned(),
        options_schema: String::new(),
        options: "{}".into(),
        ..Default::default()
    };
    let config = match kind {
        w::ConnectionKind::Llm | w::ConnectionKind::Unspecified => {
            w::connection_config::Config::Llm(Box::new(w::LlmConnection {
                endpoint: Some(ep).into(),
                ..Default::default()
            }))
        }
        w::ConnectionKind::Tts => w::connection_config::Config::Tts(Box::new(w::TtsConnection {
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
        w::ConnectionKind::Stt => w::connection_config::Config::Stt(Box::new(w::SttConnection {
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
        w::ConnectionKind::Embedding => {
            let mut emb_ep = ep;
            if emb_ep.adapter.trim().is_empty() {
                emb_ep.adapter = "openai".to_owned();
            }
            let sakura = emb_ep.adapter.eq_ignore_ascii_case("sakura")
                || super::is_sakura_endpoint(&emb_ep.endpoint);
            let (model_id, dimensions) = if sakura {
                ("multilingual-e5-large".to_owned(), 1024)
            } else {
                ("text-embedding-3-small".to_owned(), 1536)
            };
            w::connection_config::Config::Embedding(Box::new(w::EmbeddingConnection {
                endpoint: Some(emb_ep).into(),
                model_id,
                dimensions,
                ..Default::default()
            }))
        }
        w::ConnectionKind::Mcp => return None,
        w::ConnectionKind::Search => {
            let mut search_ep = ep;
            if search_ep.adapter.trim().is_empty() {
                search_ep.adapter = "mwmbl".to_owned();
            }
            w::connection_config::Config::Search(Box::new(w::SearchConnection {
                endpoint: Some(search_ep).into(),
                max_results: 5,
                ..Default::default()
            }))
        }
        w::ConnectionKind::Acp => w::connection_config::Config::Acp(Box::new(w::AcpConnection {
            command: adapter_or_cmd.to_owned(),
            args: url_or_args.split_whitespace().map(str::to_owned).collect(),
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
    };
    Some(w::ConnectionInput {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
        enabled: true,
        expected_revision: 0,
        kind: kind.into(),
        config: Some(w::ConnectionConfig {
            config: Some(config),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_a_saved_mcp_connection_replaces_only_the_requested_arguments() {
        let mcp = w::McpConnection {
            command: "npx".into(),
            args: ["-y", "old-package", r"C:\My Tools\日本語", ""]
                .map(str::to_owned)
                .to_vec(),
            allowed_tools: vec!["echo".into()],
            ..Default::default()
        };
        let conn = w::Connection {
            id: "saved-mcp".into(),
            kind: w::ConnectionKind::Mcp.into(),
            revision: 3,
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Mcp(Box::new(mcp.clone()))),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let initial = serde_json::json!(mcp.args).to_string();
        let draft = initial.replace("old-package", "new-package");
        let args = parse_process_args(&draft).unwrap();
        let input = apply_connection_edits(
            &conn,
            "Saved MCP",
            "",
            Some(("npx", &args)),
            "echo",
            None,
            None,
        );
        assert_eq!(input.expected_revision, conn.revision);
        let Some(w::connection_config::Config::Mcp(saved)) = input.config.config.as_ref() else {
            panic!("expected MCP settings");
        };
        assert_eq!(saved.command, "npx");
        assert_eq!(saved.args, ["-y", "new-package", r"C:\My Tools\日本語", ""]);
        assert_eq!(saved.allowed_tools, mcp.allowed_tools);
    }
}
