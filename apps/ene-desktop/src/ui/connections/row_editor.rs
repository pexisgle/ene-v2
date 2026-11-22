use super::mcp_editor::{McpDraftGroup, ParsedMcpExtra, append_mcp_extra_fields, read_mcp_extra};
use super::pickers::PickerInput;
use crate::input::Draft;
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    CircleButtonStyle, Tone, circle_icon_button, kind_pill, pill_button, status_pill, toggle_switch,
};
use crate::ui::{GOOGLE_AGENT_PLATFORM, Hub, SettingsStudio, default_embedding_dimensions};
use ene_client_core::{
    connections::{
        ConnectionEditSpec, apply_connection_edits, apply_mcp_extra_edits, extract_kind_list_init,
        parse_process_args,
    },
    domain::{Connection, ConnectionCheck, ConnectionConfig, ConnectionEndpoint, ConnectionKind},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

struct RowBadge {
    label: &'static str,
    dot: gpui::Hsla,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
}

struct RowHeaderFlags {
    expanded: bool,
    should_auto_probe: bool,
    live: bool,
}

struct EditorDraftSet {
    name_d: Entity<Draft>,
    ep_d: Entity<Draft>,
    list_d: Entity<Draft>,
    cmd_d: Option<Entity<Draft>>,
    mcp_args_d: Option<Entity<Draft>>,
    mcp_extra_d: Option<McpDraftGroup>,
    dim_d: Option<Entity<Draft>>,
}

struct EditorSavePayload {
    live: bool,
    can_save: bool,
    next_name: String,
    next_ep: String,
    next_cmd: Option<String>,
    next_args: Result<Vec<String>, &'static str>,
    next_list: String,
    next_dim: Option<String>,
    next_options_json: Option<String>,
    next_mcp_extra: Option<ParsedMcpExtra>,
}

impl SettingsStudio {
    pub(super) fn render_connection_row(
        &mut self,
        conn: &Connection,
        check: Option<&ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = conn.id.clone();
        let expanded = self.connection_form.expanded_connection_id.as_deref() == Some(id.as_str());
        let needs_key_flag = conn.needs_key(
            self.hub
                .read(cx)
                .model
                .connections
                .providers
                .as_deref()
                .unwrap_or_default(),
        );
        let badge = compute_connection_badge(conn, check, needs_key_flag, theme);
        let should_auto_probe = should_auto_probe_connection(conn, check, needs_key_flag);
        let header = render_row_header(
            conn,
            &badge,
            RowHeaderFlags {
                expanded,
                should_auto_probe,
                live,
            },
            theme,
            cx,
        );
        let detail = expanded.then(|| self.render_connection_editor(conn, check, live, theme, cx));

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
        conn: &Connection,
        check: Option<&ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = conn.id.clone();
        let endpoint = conn.endpoint();
        let agent_platform = conn.kind == ConnectionKind::Llm
            && endpoint.is_some_and(|ep| ep.adapter == GOOGLE_AGENT_PLATFORM);

        let drafts = self.collect_editor_drafts(conn, endpoint, theme, cx);
        let mcp_tools_strip = self.render_mcp_tools_strip(conn, check, &drafts.list_d, theme, cx);
        let dropdown_picker = self.render_kind_dropdown_picker(
            PickerInput {
                conn,
                endpoint,
                list_d: &drafts.list_d,
                dim_d: drafts.dim_d.as_ref(),
                live,
            },
            theme,
            cx,
        );
        let (schema_controls, next_options_json) = if agent_platform {
            self.render_agent_platform_options(&id, conn.revision, endpoint, theme, cx)
        } else {
            self.render_options_schema(&id, conn.revision, endpoint, theme, cx)
        };

        let payload = build_editor_save_payload(&drafts, next_options_json, live, cx);
        let args_error = payload.next_args.as_ref().err().copied();
        let mcp_kv_error = payload
            .next_mcp_extra
            .as_ref()
            .and_then(|(_, _, _, env_r, hdr_r)| {
                env_r
                    .as_ref()
                    .err()
                    .or_else(|| hdr_r.as_ref().err())
                    .copied()
            });
        let show_list_field = agent_platform
            || (!is_voicevox_tts(conn, endpoint)
                && !(conn.kind == ConnectionKind::Tts && conn.is_sakura())
                && !matches!(conn.kind, ConnectionKind::Llm | ConnectionKind::Embedding));

        let cred_strip = self.render_credentials_strip(conn, live, theme, cx);
        let actions = render_editor_actions(conn, payload, &self.hub, theme);

        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(16.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(render_enable_row(conn, live, &self.hub, theme))
            .when_some(error_banner(check, theme), |this, strip| this.child(strip))
            .child(cred_strip)
            .child(labeled_field("表示名", drafts.name_d, theme))
            .when_some(drafts.cmd_d, |this, cd| {
                this.child(labeled_field("コマンド", cd, theme))
            })
            .when_some(drafts.mcp_args_d, |this, draft| {
                append_mcp_args_field(this, &id, draft, args_error, theme)
            })
            .when_some(drafts.mcp_extra_d, |this, group| {
                append_mcp_extra_fields(this, &id, group, mcp_kv_error, theme)
            })
            .when(!agent_platform, |this| {
                this.child(labeled_field("エンドポイント URL", drafts.ep_d, theme))
            })
            .when_some(dropdown_picker, |this, picker| this.child(picker))
            .when_some(drafts.dim_d, |this, dd| {
                this.child(labeled_field("次元数 (dimensions)", dd, theme))
            })
            .when(show_list_field, |this| {
                this.child(labeled_field(
                    conn.kind.list_field_label(),
                    drafts.list_d,
                    theme,
                ))
            })
            .when_some(mcp_tools_strip, |this, strip| this.child(strip))
            .when(!schema_controls.is_empty(), |this| {
                this.child(render_schema_box(schema_controls, theme))
            })
            .child(actions)
            .into_any_element()
    }

    fn collect_editor_drafts(
        &mut self,
        conn: &Connection,
        endpoint: Option<&ConnectionEndpoint>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> EditorDraftSet {
        let id = &conn.id;
        let rev = conn.revision;
        let cfg = conn.config.as_ref();
        let list_init = extract_kind_list_init(cfg);
        EditorDraftSet {
            name_d: self.draft(
                cx,
                format!("conn:{id}:r{rev}:name"),
                "接続名",
                Some(&conn.name),
                theme,
            ),
            ep_d: self.draft(
                cx,
                format!("conn:{id}:r{rev}:ep"),
                "https://...",
                endpoint.map(|e| e.endpoint.as_str()),
                theme,
            ),
            list_d: self.draft(
                cx,
                format!("conn:{id}:r{rev}:list"),
                "",
                Some(&list_init),
                theme,
            ),
            cmd_d: self.process_command_draft(conn, cfg, theme, cx),
            mcp_args_d: self.mcp_args_draft(id, rev, cfg, theme, cx),
            mcp_extra_d: self.mcp_extra_drafts(id, rev, cfg, theme, cx),
            dim_d: self.embedding_dim_draft(conn, cfg, &list_init, theme, cx),
        }
    }

    fn embedding_dim_draft(
        &mut self,
        conn: &Connection,
        cfg: Option<&ConnectionConfig>,
        list_init: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Draft>> {
        if conn.kind != ConnectionKind::Embedding {
            return None;
        }
        let init = match cfg {
            Some(ConnectionConfig::Embedding(emb)) if emb.dimensions > 0 => {
                emb.dimensions.to_string()
            }
            _ => default_embedding_dimensions(list_init).to_string(),
        };
        Some(self.draft(
            cx,
            format!("conn:{}:r{}:dims", conn.id, conn.revision),
            "例: 1024",
            Some(&init),
            theme,
        ))
    }

    fn render_credentials_strip(
        &mut self,
        conn: &Connection,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let id = conn.id.clone();
        let revealed = self.connection_form.revealed_secret_for.as_deref() == Some(id.as_str());
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
        let sakura_ids = collect_sakura_conn_ids(conn, self.hub.read(cx));
        let revoke_armed = self.connection_form.revoke_armed_for.as_deref() == Some(id.as_str());

        div()
            .rounded(px(10.))
            .bg(theme.bg_blush)
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(render_cred_strip_header(cred_label, has_cred, theme))
            .child(render_cred_secret_row(
                CredSecretInput {
                    id: &id,
                    secret_d,
                    secret_val,
                    revealed,
                    live,
                },
                (&sakura_ids, &self.hub),
                theme,
                cx,
            ))
            .when(conn.credential_state == "registered", |this| {
                this.child(render_revoke_controls(
                    &id,
                    revoke_armed,
                    live,
                    &self.hub,
                    theme,
                    cx,
                ))
            })
    }
}

pub(super) fn is_voicevox_tts(conn: &Connection, endpoint: Option<&ConnectionEndpoint>) -> bool {
    conn.kind == ConnectionKind::Tts
        && !conn.is_sakura()
        && endpoint.is_some_and(|ep| ep.adapter.eq_ignore_ascii_case("voicevox"))
}

fn build_editor_save_payload(
    drafts: &EditorDraftSet,
    next_options_json: Option<String>,
    live: bool,
    cx: &gpui::App,
) -> EditorSavePayload {
    let next_name = drafts.name_d.read(cx).text().trim().to_owned();
    let next_ep = drafts.ep_d.read(cx).text().trim().to_owned();
    let next_list = drafts.list_d.read(cx).text().trim().to_owned();
    let next_dim = drafts
        .dim_d
        .as_ref()
        .map(|d| d.read(cx).text().trim().to_owned());
    let next_cmd = drafts
        .cmd_d
        .as_ref()
        .map(|d| d.read(cx).text().trim().to_owned());
    let next_args = drafts.mcp_args_d.as_ref().map_or(Ok(Vec::new()), |draft| {
        parse_process_args(draft.read(cx).text())
    });
    let next_mcp_extra = drafts
        .mcp_extra_d
        .as_ref()
        .map(|group| read_mcp_extra(group, cx));
    let mcp_kv_ok = next_mcp_extra
        .as_ref()
        .is_none_or(|(_, _, _, env_r, hdr_r)| env_r.is_ok() && hdr_r.is_ok());

    EditorSavePayload {
        live,
        can_save: live && !next_name.is_empty() && next_args.is_ok() && mcp_kv_ok,
        next_name,
        next_ep,
        next_cmd,
        next_args,
        next_list,
        next_dim,
        next_options_json,
        next_mcp_extra,
    }
}

fn append_mcp_args_field(
    panel: gpui::Div,
    id: &str,
    draft: Entity<Draft>,
    args_error: Option<&'static str>,
    theme: &Theme,
) -> gpui::Div {
    panel
        .child(labeled_field(
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
}

fn render_schema_box(schema_controls: Vec<AnyElement>, theme: &Theme) -> gpui::Div {
    div()
        .rounded(px(10.))
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .children(schema_controls)
}

fn render_cred_strip_header(cred_label: &'static str, has_cred: bool, theme: &Theme) -> gpui::Div {
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
        ))
}

struct CredSecretInput<'a> {
    id: &'a str,
    secret_d: Entity<Draft>,
    secret_val: String,
    revealed: bool,
    live: bool,
}

fn render_cred_secret_row(
    input: CredSecretInput<'_>,
    (sakura_ids, hub): (&[String], &Entity<Hub>),
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let CredSecretInput {
        id,
        secret_d,
        secret_val,
        revealed,
        live,
    } = input;
    let studio_eye = cx.entity();
    let eye_id = id.to_owned();
    let hub_reg = hub.clone();
    let reg_id = id.to_owned();
    let secret_save = secret_val.clone();
    let ids_all = sakura_ids.to_vec();
    let hub_all = hub.clone();

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(div().flex_1().min_w_0().child(secret_d))
        .child(circle_icon_button(
            SharedString::from(format!("conn-eye-{id}")),
            CircleButtonStyle::new(if revealed { Icon::EyeOff } else { Icon::Eye }, 30., 14.),
            theme,
            move |_, cx| {
                studio_eye.update(cx, |this, cx| {
                    this.connection_form.revealed_secret_for =
                        if revealed { None } else { Some(eye_id.clone()) };
                    cx.notify();
                });
            },
        ))
        .when(ids_all.len() > 1, |this| {
            let sec_all = secret_val.clone();
            this.child(pill_button(
                SharedString::from(format!("conn-save-sakura-all-{id}")),
                "全さくら接続に登録",
                Tone::Soft
                    .icon(Icon::Check)
                    .enabled(live && !sec_all.is_empty()),
                theme,
                move |_, cx| {
                    hub_all.update(cx, |hub, cx| {
                        hub.register_secret_for_connections(ids_all.clone(), sec_all.clone(), cx);
                    });
                },
            ))
        })
        .child(pill_button(
            SharedString::from(format!("conn-save-key-{id}")),
            "キーを登録",
            Tone::Primary
                .icon(Icon::Check)
                .enabled(live && !secret_save.is_empty()),
            theme,
            move |_, cx| {
                hub_reg.update(cx, |hub, cx| {
                    hub.register_secret(reg_id.clone(), secret_save.clone(), cx);
                });
            },
        ))
}

fn compute_connection_badge(
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    needs_key_flag: bool,
    theme: &Theme,
) -> RowBadge {
    let is_unsupported_check = check
        .is_some_and(|c| !c.healthy && c.failure.as_ref().is_some_and(|f| f.code == "unsupported"));
    if !conn.enabled {
        return RowBadge {
            label: "無効",
            dot: theme.ink_placeholder,
            bg: theme.bg_blush,
            fg: theme.ink_muted,
        };
    }
    if needs_key_flag {
        return RowBadge {
            label: "キー登録が必要",
            dot: theme.amber,
            bg: theme.amber_soft,
            fg: theme.amber_fg,
        };
    }
    if is_unsupported_check {
        return RowBadge {
            label: "設定済み",
            dot: theme.mint,
            bg: theme.mint_soft,
            fg: theme.mint_fg,
        };
    }
    match check.map(|c| c.healthy) {
        Some(true) => RowBadge {
            label: "接続先を確認済み",
            dot: theme.mint,
            bg: theme.mint_soft,
            fg: theme.mint_fg,
        },
        Some(false) => RowBadge {
            label: "接続エラー",
            dot: theme.danger,
            bg: theme.danger_soft,
            fg: theme.danger_fg,
        },
        None => RowBadge {
            label: "未確認",
            dot: theme.amber,
            bg: theme.amber_soft,
            fg: theme.amber_fg,
        },
    }
}

fn should_auto_probe_connection(
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    needs_key_flag: bool,
) -> bool {
    let kind = conn.kind;
    let ep = conn.endpoint();
    let is_voicevox = is_voicevox_tts(conn, ep);
    let agent_platform = conn
        .llm_endpoint()
        .is_some_and(|e| e.adapter == GOOGLE_AGENT_PLATFORM);
    conn.enabled
        && !needs_key_flag
        && (kind == ConnectionKind::Llm || is_voicevox)
        && check.is_none_or(|c| {
            c.resources.is_empty()
                || (agent_platform
                    && !c
                        .capabilities
                        .iter()
                        .any(|cap| cap == "verified_model_list"))
        })
}

fn render_row_header(
    conn: &Connection,
    badge: &RowBadge,
    flags: RowHeaderFlags,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let RowHeaderFlags {
        expanded,
        should_auto_probe,
        live,
    } = flags;
    let id = &conn.id;
    let kind = conn.kind;
    let is_checking = cx
        .entity()
        .read(cx)
        .hub
        .read(cx)
        .checking_connections
        .contains(id);
    let ep = conn.endpoint();
    let is_voicevox = is_voicevox_tts(conn, ep);
    let hub_check = cx.entity().read(cx).hub.clone();
    let hub_expand_probe = hub_check.clone();
    let check_id = id.clone();
    let expand_id = id.clone();
    let studio_expand = cx.entity();

    div()
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
                .child(kind_pill(kind.label(), theme))
                .child(status_pill(
                    badge.label,
                    Some(badge.dot),
                    badge.bg,
                    badge.fg,
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
                    Tone::Ghost
                        .icon(Icon::Refresh)
                        .enabled(live && !is_checking),
                    theme,
                    move |_, cx| {
                        hub_check.update(cx, |hub, cx| hub.check_connection(check_id.clone(), cx));
                    },
                ))
                .child(pill_button(
                    SharedString::from(format!("conn-edit-{id}")),
                    if expanded {
                        "閉じる"
                    } else if badge.label == "キー登録が必要" {
                        "キーを登録"
                    } else {
                        "編集"
                    },
                    if expanded { Tone::Soft } else { Tone::Ghost },
                    theme,
                    move |_, cx| {
                        let mut probe_id = None;
                        studio_expand.update(cx, |this, cx| {
                            if this.connection_form.expanded_connection_id.as_deref()
                                == Some(expand_id.as_str())
                            {
                                this.connection_form.expanded_connection_id = None;
                            } else {
                                this.connection_form.expanded_connection_id =
                                    Some(expand_id.clone());
                                if should_auto_probe
                                    && this
                                        .connection_form
                                        .auto_probed_connections
                                        .insert(expand_id.clone())
                                {
                                    probe_id = Some(expand_id.clone());
                                }
                            }
                            this.open_dropdown = None;
                            this.connection_form.revoke_armed_for = None;
                            cx.notify();
                        });
                        if let Some(pid) = probe_id {
                            hub_expand_probe.update(cx, |hub, cx| hub.check_connection(pid, cx));
                        }
                    },
                )),
        )
}

fn collect_sakura_conn_ids(conn: &Connection, hub: &Hub) -> Vec<String> {
    if !conn.is_sakura() {
        return Vec::new();
    }
    hub.model
        .settings()
        .map(|s| {
            s.connections
                .iter()
                .filter(|c| c.is_sakura())
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn render_revoke_controls(
    id: &str,
    revoke_armed: bool,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let studio = cx.entity();
    let hub_rev = hub.clone();
    let own_id = id.to_owned();
    let row = div().flex().flex_row().justify_end().gap(px(6.));
    if revoke_armed {
        let cancel_studio = studio.clone();
        row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(pill_button(
                    SharedString::from(format!("conn-rev-cancel-{id}")),
                    "キャンセル",
                    Tone::Ghost,
                    theme,
                    move |_, cx| {
                        cancel_studio.update(cx, |this, cx| {
                            this.connection_form.revoke_armed_for = None;
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    SharedString::from(format!("conn-rev-exec-{id}")),
                    "キーを削除",
                    Tone::Danger.icon(Icon::Trash).enabled(live),
                    theme,
                    move |_, cx| {
                        hub_rev.update(cx, |hub, cx| hub.revoke_secret(own_id.clone(), cx))
                    },
                )),
        )
    } else {
        row.child(pill_button(
            SharedString::from(format!("conn-rev-arm-{id}")),
            "キーを削除",
            Tone::Danger.icon(Icon::Trash).enabled(live),
            theme,
            move |_, cx| {
                studio.update(cx, |this, cx| {
                    this.connection_form.revoke_armed_for = Some(own_id.clone());
                    cx.notify();
                });
            },
        ))
    }
}

fn render_enable_row(conn: &Connection, live: bool, hub: &Entity<Hub>, theme: &Theme) -> gpui::Div {
    let conn_toggle = conn.clone();
    let hub_toggle = hub.clone();
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
            SharedString::from(format!("conn-enable-{}", conn.id)),
            conn.enabled,
            live,
            theme,
            move |_, cx| {
                hub_toggle.update(cx, |hub, cx| {
                    hub.set_connection_enabled(conn_toggle.clone(), !conn_toggle.enabled, cx);
                });
            },
        ))
}

fn error_banner(check: Option<&ConnectionCheck>, theme: &Theme) -> Option<gpui::Div> {
    check
        .filter(|c| !c.healthy)
        .and_then(|c| c.failure.as_ref())
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
        })
}

fn render_editor_actions(
    conn: &Connection,
    payload: EditorSavePayload,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let id = &conn.id;
    let rev = conn.revision;
    let delete_id = id.clone();
    let delete_title = format!("「{}」を削除しますか？", conn.name);
    let hub_delete = hub.clone();
    let hub_save = hub.clone();
    let conn_for_save = conn.clone();

    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap(px(8.))
        .child(pill_button(
            SharedString::from(format!("conn-delete-{id}")),
            "接続を削除",
            Tone::Danger.icon(Icon::Trash).enabled(payload.live),
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
                })
                .detach();
            },
        ))
        .child(pill_button(
            SharedString::from(format!("conn-save-{id}")),
            "保存",
            Tone::Primary.icon(Icon::Check).enabled(payload.can_save),
            theme,
            move |_, cx| {
                let Ok(args) = &payload.next_args else {
                    return;
                };
                let mut input = apply_connection_edits(
                    &conn_for_save,
                    &ConnectionEditSpec {
                        name: &payload.next_name,
                        endpoint_url: &payload.next_ep,
                        process: payload.next_cmd.as_deref().map(|cmd| (cmd, args.as_slice())),
                        list_raw: &payload.next_list,
                        dimensions_raw: payload.next_dim.as_deref(),
                        options_json: payload.next_options_json.as_deref(),
                    },
                );
                if let Some((cwd, secret_env, auth_hdr, Ok(env), Ok(headers))) =
                    &payload.next_mcp_extra
                {
                    apply_mcp_extra_edits(&mut input, cwd, secret_env, auth_hdr, env, headers);
                }
                hub_save.update(cx, |hub, cx| hub.upsert_connection("接続設定の保存", input, cx));
            },
        ))
}
