//! Connection editor drafts, validation and save-payload building.

use super::ConnectionsPane;
use crate::input::Draft;
use crate::ui::connections::mcp_editor::{mcp_args_draft, mcp_extra_drafts, process_command_draft};
use crate::ui::connections::{mcp_fields::McpFields, secret_row};
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill};
use crate::ui::{Hub, default_embedding_dimensions};
use ene_client_core::{
    connections::{extract_kind_list_init, parse_process_args},
    domain::{Connection, ConnectionCheck, ConnectionConfig, ConnectionEndpoint, ConnectionKind},
};
use gpui::{Context, Entity, FontWeight, SharedString, div, prelude::*, px};

pub(crate) struct RowBadge {
    pub(super) label: &'static str,
    pub(super) dot: gpui::Hsla,
    pub(super) bg: gpui::Hsla,
    pub(super) fg: gpui::Hsla,
}

#[derive(Clone, Copy)]
pub(crate) struct RowHeaderFlags {
    pub(super) expanded: bool,
    pub(super) live: bool,
}

pub(crate) struct EditorDraftSet {
    pub(super) name_d: Entity<Draft>,
    pub(super) ep_d: Entity<Draft>,
    pub(super) list_d: Entity<Draft>,
    pub(super) cmd_d: Option<Entity<Draft>>,
    pub(super) mcp_args_d: Option<Entity<Draft>>,
    pub(super) mcp_extra_d: Option<McpFields>,
    pub(super) dim_d: Option<Entity<Draft>>,
}

pub(crate) struct EditorSavePayload {
    pub(super) live: bool,
    pub(super) can_save: bool,
    pub(super) next_name: String,
    pub(super) next_ep: String,
    pub(super) next_cmd: Option<String>,
    pub(super) next_args: Result<Vec<String>, &'static str>,
    pub(super) next_list: String,
    pub(super) next_dim: Option<String>,
    pub(super) next_options_json: Option<String>,
    pub(super) next_mcp_extra:
        Option<Result<ene_client_core::connections::McpExtraConfig, &'static str>>,
}

pub(super) fn collect_editor_drafts(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    endpoint: Option<&ConnectionEndpoint>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> EditorDraftSet {
    let id = &conn.id;
    let rev = conn.revision;
    let cfg = conn.config.as_ref();
    let list_init = extract_kind_list_init(cfg);
    EditorDraftSet {
        name_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:name"),
            "接続名",
            Some(&conn.name),
            theme,
        ),
        ep_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:ep"),
            "https://...",
            endpoint.map(|e| e.endpoint.as_str()),
            theme,
        ),
        list_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:list"),
            "",
            Some(&list_init),
            theme,
        ),
        cmd_d: process_command_draft(pane, conn, cfg, theme, cx),
        mcp_args_d: mcp_args_draft(pane, id, rev, cfg, theme, cx),
        mcp_extra_d: mcp_extra_drafts(pane, id, rev, cfg, theme, cx),
        dim_d: embedding_dim_draft(pane, conn, cfg, &list_init, theme, cx),
    }
}

pub(super) fn embedding_dim_draft(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    cfg: Option<&ConnectionConfig>,
    list_init: &str,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<Entity<Draft>> {
    if conn.kind != ConnectionKind::Embedding {
        return None;
    }
    let init = match cfg {
        Some(ConnectionConfig::Embedding(emb)) if emb.dimensions > 0 => emb.dimensions.to_string(),
        _ => default_embedding_dimensions(list_init).to_string(),
    };
    Some(pane.drafts.draft(
        cx,
        format!("conn:{}:r{}:dims", conn.id, conn.revision),
        "例: 1024",
        Some(&init),
        theme,
    ))
}

pub(super) fn build_editor_save_payload(
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
    let next_mcp_extra = drafts.mcp_extra_d.as_ref().map(|group| group.read(cx));
    let mcp_kv_ok = next_mcp_extra.as_ref().is_none_or(Result::is_ok);

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

pub(super) fn append_mcp_args_field(
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

pub(super) fn render_cred_strip_header(
    cred_label: &'static str,
    has_cred: bool,
    theme: &Theme,
) -> gpui::Div {
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

pub(crate) struct CredSecretInput<'a> {
    pub(super) id: &'a str,
    pub(super) secret_d: Entity<Draft>,
    pub(super) secret_val: String,
    pub(super) revealed: bool,
    pub(super) live: bool,
}

pub(super) fn render_cred_secret_row(
    input: CredSecretInput<'_>,
    (sakura_ids, hub): (&[String], &Entity<Hub>),
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let CredSecretInput {
        id,
        secret_d,
        secret_val,
        revealed,
        live,
    } = input;
    let hub_reg = hub.clone();
    let reg_id = id.to_owned();
    let secret_save = secret_val.clone();
    let ids_all = sakura_ids.to_vec();
    let hub_all = hub.clone();
    let revision = hub
        .read(cx)
        .model
        .settings()
        .map(|settings| settings.revision);

    secret_row(
        format!("conn-eye-{id}"),
        id.to_owned(),
        secret_d,
        revealed,
        theme,
        cx,
    )
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
                    if let Some(revision) = revision {
                        hub.register_secret_for_connections(
                            ids_all.clone(),
                            &sec_all,
                            revision,
                            cx,
                        );
                    }
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
                if let Some(revision) = revision {
                    hub.register_secret(&reg_id, &secret_save, Some(revision), cx);
                }
            });
        },
    ))
}

pub(super) fn compute_connection_badge(
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    needs_key_flag: bool,
    theme: &Theme,
) -> RowBadge {
    let neutral = (theme.ink_placeholder, theme.bg_blush, theme.ink_muted);
    let pending = (theme.amber, theme.amber_soft, theme.amber_fg);
    let ready = (theme.mint, theme.mint_soft, theme.mint_fg);
    let failed = (theme.danger, theme.danger_soft, theme.danger_fg);
    let (label, (dot, bg, fg)) = match check {
        _ if !conn.enabled => ("無効", neutral),
        _ if needs_key_flag => ("キー登録が必要", pending),
        Some(check) if check.healthy => {
            let label = if conn.kind == ConnectionKind::Llm && check.checked_model.is_none() {
                "モデル一覧取得済み"
            } else {
                "接続先を確認済み"
            };
            (label, ready)
        }
        Some(check)
            if check
                .failure
                .as_ref()
                .is_some_and(|failure| failure.code == "unsupported") =>
        {
            ("設定済み", ready)
        }
        Some(_) => ("接続エラー", failed),
        None => ("未確認", pending),
    };
    RowBadge { label, dot, bg, fg }
}

pub(super) fn collect_sakura_conn_ids(conn: &Connection, hub: &Hub) -> Vec<String> {
    if !conn.is_sakura() {
        return Vec::new();
    }
    hub.model.settings().map_or_default(|s| {
        s.connections
            .iter()
            .filter(|c| c.is_sakura())
            .map(|c| c.id.clone())
            .collect()
    })
}
