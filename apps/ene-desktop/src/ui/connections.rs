//! Connections chapter in `SettingsStudio`: single-line 62px rows with status pills,
//! 1-level hairline expansion, dynamic `options_schema`, MCP tool selector,
//! and bottom collapsed disclosure for 7 model-role slot overrides.

use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{
    Tone, circle_icon_button, disclosure_row, kind_pill, pill_button, status_pill, toggle_switch,
};
use super::{ModelRoleSlot, SettingsStudio};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};
use std::collections::BTreeMap;

impl SettingsStudio {
    pub(crate) fn render_connections(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (live, connections, providers, checks, companion, all_models) = {
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
            )
        };

        let studio_add = cx.entity();
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
                        cx.notify();
                    });
                },
            ));

        let add_panel = if adding {
            Some(self.render_add_connection_panel(live, &providers, theme, cx))
        } else {
            None
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        for conn in &connections {
            rows.push(self.render_connection_row(
                conn,
                checks.get(&conn.id).or_else(|| conn.last_check.as_option()),
                live,
                theme,
                cx,
            ));
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
            Some(self.render_advanced_roles_panel(companion.as_ref(), &all_models, live, theme, cx))
        } else {
            None
        };

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(add_panel, |this, p| this.child(p))
            .child(div().flex().flex_col().gap(px(10.)).children(rows))
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

    fn render_add_connection_panel(
        &mut self,
        live: bool,
        providers: &[w::LlmProvider],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        let hub_oai_tts = self.hub.clone();
        let hub_stt = self.hub.clone();
        let studio_vv = cx.entity();
        let studio_oai = cx.entity();
        let studio_stt = cx.entity();

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
            ));

        let custom_kind = self.custom_kind;
        let mut kind_chips = Vec::new();
        for kind in [
            w::ConnectionKind::Llm,
            w::ConnectionKind::Tts,
            w::ConnectionKind::Stt,
            w::ConnectionKind::Embedding,
            w::ConnectionKind::Mcp,
            w::ConnectionKind::Acp,
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

        let name_d = self.draft(cx, "add:custom:name", "表示名", None, theme);
        let adapter_d = self.draft(
            cx,
            "add:custom:adapter",
            "アダプタ名またはコマンド（例: openai / npx）",
            None,
            theme,
        );
        let target_d = self.draft(
            cx,
            "add:custom:target",
            "エンドポイントURLまたは引数（スペース区切り）",
            None,
            theme,
        );
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
                            .child("音声機能を追加"),
                    )
                    .child(voice_chips),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink_muted)
                            .child("カスタム接続"),
                    )
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
                                "アダプタ / コマンド",
                                adapter_d,
                                theme,
                            )))
                            .child(div().flex_1().min_w_0().child(labeled_field(
                                "URL / 引数",
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
                            let input = build_custom_connection_input(
                                custom_kind,
                                &c_name,
                                &c_adapter,
                                &c_target,
                            );
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

        let (state_label, state_dot, state_bg, state_fg) = if !conn.enabled {
            (
                "無効",
                theme.ink_placeholder,
                theme.bg_blush,
                theme.ink_muted,
            )
        } else {
            match check.map(|c| c.healthy) {
                Some(true) => ("接続中", theme.mint, theme.mint_soft, theme.mint_fg),
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
        let check_id = id.clone();
        let studio_expand = cx.entity();
        let expand_id = id.clone();

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
                        "確認",
                        Some(Icon::Refresh),
                        Tone::Ghost,
                        live,
                        theme,
                        move |_, cx| {
                            hub_check.update(cx, |hub, cx| {
                                hub.check_connection(check_id.clone(), cx);
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("conn-edit-{id}")),
                        if expanded { "閉じる" } else { "編集" },
                        None,
                        if expanded { Tone::Soft } else { Tone::Ghost },
                        true,
                        theme,
                        move |_, cx| {
                            studio_expand.update(cx, |this, cx| {
                                if this.expanded_connection_id.as_deref()
                                    == Some(expand_id.as_str())
                                {
                                    this.expanded_connection_id = None;
                                } else {
                                    this.expanded_connection_id = Some(expand_id.clone());
                                }
                                this.revoke_armed_for = None;
                                cx.notify();
                            });
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

        let err_strip = check
            .filter(|c| !c.healthy)
            .and_then(|c| c.failure.as_option())
            .filter(|f| !f.message.trim().is_empty())
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
            w::ConnectionKind::Mcp => "許可ツール（カンマ区切り）",
            w::ConnectionKind::Acp => "起動引数（スペース区切り）",
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

        // MCP tool filter when > 20 capabilities
        let mcp_tools_strip = if kind_of(conn) == w::ConnectionKind::Mcp
            && let Some(caps) = check.map(|c| &c.capabilities).filter(|c| !c.is_empty())
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

        // Dynamic options_schema fields
        let (schema_controls, next_options_json) =
            self.render_options_schema(&id, rev, endpoint.as_ref(), theme, cx);

        let next_name = name_d.read(cx).text().trim().to_owned();
        let next_ep = ep_d.read(cx).text().trim().to_owned();
        let next_list = list_d.read(cx).text().trim().to_owned();
        let next_cmd = cmd_d.as_ref().map(|d| d.read(cx).text().trim().to_owned());

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
            .child(labeled_field("エンドポイント URL", ep_d, theme))
            .child(labeled_field(list_label, list_d, theme))
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
            .child(div().flex().flex_row().justify_end().child(pill_button(
                SharedString::from(format!("conn-save-{id}")),
                "保存",
                Some(Icon::Check),
                Tone::Primary,
                live && !next_name.is_empty(),
                theme,
                move |_, cx| {
                    let input = apply_connection_edits(
                        &conn_for_save,
                        &next_name,
                        &next_ep,
                        next_cmd.as_deref(),
                        &next_list,
                        next_options_json.as_deref(),
                    );
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
        &self,
        companion: Option<&w::Companion>,
        all_models: &[(String, String, String)],
        live: bool,
        theme: &Theme,
        _cx: &mut Context<Self>,
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
        let mut by_conn: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (cid, mid, label) in all_models {
            by_conn
                .entry(cid.clone())
                .or_default()
                .push((mid.clone(), label.clone()));
        }

        let mut rows = Vec::new();
        for (idx, slot) in ModelRoleSlot::ALL.into_iter().enumerate() {
            let current = slot.current(&assignments);
            let current_text = current
                .as_ref()
                .map(|m| m.model_id.clone())
                .unwrap_or_else(|| slot.fallback_hint().to_owned());

            let mut chips = Vec::new();
            for (cid, mid, label) in all_models {
                let selected = current
                    .as_ref()
                    .is_some_and(|c| c.connection_id == *cid && c.model_id == *mid);
                let hub_assign = self.hub.clone();
                let next_cid = cid.clone();
                let next_mid = mid.clone();
                chips.push(pill_button(
                    SharedString::from(format!("adv-role-{slot:?}-{cid}-{mid}")),
                    label.clone(),
                    selected.then_some(Icon::Check),
                    if selected { Tone::Primary } else { Tone::Ghost },
                    live && !selected,
                    theme,
                    move |_, cx| {
                        hub_assign.update(cx, |hub, cx| {
                            hub.assign_model(slot, Some((next_cid.clone(), next_mid.clone())), cx);
                        });
                    },
                ));
            }
            if current.is_some() && !matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task)
            {
                let hub_clear = self.hub.clone();
                chips.push(pill_button(
                    SharedString::from(format!("adv-role-clear-{slot:?}")),
                    "自動に戻す",
                    None,
                    Tone::Soft,
                    live,
                    theme,
                    move |_, cx| hub_clear.update(cx, |hub, cx| hub.assign_model(slot, None, cx)),
                ));
            }

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
                                    .child(current_text),
                            ),
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
            .children(rows)
            .into_any_element()
    }
}

fn kind_label(kind: w::ConnectionKind) -> &'static str {
    match kind {
        w::ConnectionKind::Llm => "LLM",
        w::ConnectionKind::Tts => "音声合成",
        w::ConnectionKind::Stt => "音声認識",
        w::ConnectionKind::Embedding => "埋め込み",
        w::ConnectionKind::Mcp => "MCP",
        w::ConnectionKind::Acp => "ACP",
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
        None => String::new(),
    }
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
    command: Option<&str>,
    list_raw: &str,
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
            tts.endpoint = updated_ep(tts.endpoint.as_option());
            tts.voice_ids = split_comma(list_raw);
        }
        Some(w::connection_config::Config::Stt(stt)) => {
            stt.endpoint = updated_ep(stt.endpoint.as_option());
            stt.language_hints = split_comma(list_raw);
        }
        Some(w::connection_config::Config::Embedding(emb)) => {
            emb.endpoint = updated_ep(emb.endpoint.as_option());
            emb.model_id = list_raw.trim().to_owned();
        }
        Some(w::connection_config::Config::Mcp(mcp)) => {
            mcp.endpoint = updated_ep(mcp.endpoint.as_option());
            if let Some(cmd) = command {
                mcp.command = cmd.to_owned();
            }
            mcp.allowed_tools = split_comma(list_raw);
        }
        Some(w::connection_config::Config::Acp(acp)) => {
            acp.endpoint = updated_ep(acp.endpoint.as_option());
            if let Some(cmd) = command {
                acp.command = cmd.to_owned();
            }
            acp.args = list_raw.split_whitespace().map(str::to_owned).collect();
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
) -> w::ConnectionInput {
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
            w::connection_config::Config::Embedding(Box::new(w::EmbeddingConnection {
                endpoint: Some(ep).into(),
                ..Default::default()
            }))
        }
        w::ConnectionKind::Mcp => w::connection_config::Config::Mcp(Box::new(w::McpConnection {
            command: adapter_or_cmd.to_owned(),
            args: url_or_args.split_whitespace().map(str::to_owned).collect(),
            endpoint: Some(w::ConnectionEndpoint {
                endpoint: if url_or_args.starts_with("http") {
                    url_or_args.to_owned()
                } else {
                    String::new()
                },
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })),
        w::ConnectionKind::Acp => w::connection_config::Config::Acp(Box::new(w::AcpConnection {
            command: adapter_or_cmd.to_owned(),
            args: url_or_args.split_whitespace().map(str::to_owned).collect(),
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
    };
    w::ConnectionInput {
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
    }
}
