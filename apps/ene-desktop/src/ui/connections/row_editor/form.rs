//! Connection editor drafts, validation and save-payload building.

use super::ConnectionsPane;
use crate::input::Draft;
use crate::ui::connections::mcp_editor::{
    McpDraftGroup, ParsedMcpExtra, mcp_args_draft, mcp_extra_drafts, process_command_draft,
    read_mcp_extra,
};
use crate::ui::connections::secret_row;
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill};
use crate::ui::{GOOGLE_AGENT_PLATFORM, Hub, default_embedding_dimensions};
use ene_client_core::{
    connections::{ConnectionResource, extract_kind_list_init, parse_process_args},
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
    pub(super) should_auto_probe: bool,
    pub(super) live: bool,
}

pub(crate) struct EditorDraftSet {
    pub(super) name_d: Entity<Draft>,
    pub(super) ep_d: Entity<Draft>,
    pub(super) list_d: Entity<Draft>,
    pub(super) cmd_d: Option<Entity<Draft>>,
    pub(super) mcp_args_d: Option<Entity<Draft>>,
    pub(super) mcp_extra_d: Option<McpDraftGroup>,
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
    pub(super) next_mcp_extra: Option<ParsedMcpExtra>,
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
                    hub.register_secret_for_connections(ids_all.clone(), &sec_all, cx);
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
                hub.register_secret(&reg_id, &secret_save, cx);
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

pub(super) fn should_auto_probe_connection(
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    needs_key_flag: bool,
) -> bool {
    let kind = conn.kind;
    let is_voicevox = ConnectionResource::for_connection(conn) == Some(ConnectionResource::Speaker);
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
