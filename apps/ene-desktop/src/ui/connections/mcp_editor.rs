use super::ConnectionsPane;
use crate::input::Draft;
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button};
use ene_client_core::{
    connections::{McpExtraConfig, format_kv_lines, parse_mcp_kv, split_comma, toggle_comma_item},
    domain::{Connection, ConnectionCheck, ConnectionConfig, ConnectionKind},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

pub(super) struct McpDraftGroup {
    pub(super) cwd_d: Entity<Draft>,
    pub(super) secret_env_d: Entity<Draft>,
    pub(super) auth_hdr_d: Entity<Draft>,
    pub(super) env_d: Entity<Draft>,
    pub(super) headers_d: Entity<Draft>,
}

pub(super) fn process_command_draft(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    cfg: Option<&ConnectionConfig>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<Entity<Draft>> {
    let Some(ConnectionConfig::Mcp(mcp)) = cfg else {
        return None;
    };
    Some(pane.drafts.draft(
        cx,
        format!("conn:{}:r{}:cmd", conn.id, conn.revision),
        "実行コマンド",
        Some(mcp.command.as_str()),
        theme,
    ))
}

pub(super) fn mcp_args_draft(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    cfg: Option<&ConnectionConfig>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<Entity<Draft>> {
    let Some(ConnectionConfig::Mcp(mcp)) = cfg else {
        return None;
    };
    let initial = serde_json::json!(mcp.args).to_string();
    Some(pane.drafts.description_draft(
        cx,
        format!("conn:{id}:r{rev}:mcp-args"),
        r#"["-y", "@scope/mcp-server", "--stdio"]"#,
        Some(&initial),
        theme,
    ))
}

pub(super) fn mcp_extra_drafts(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    cfg: Option<&ConnectionConfig>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<McpDraftGroup> {
    let Some(ConnectionConfig::Mcp(mcp)) = cfg else {
        return None;
    };
    let env_init = format_kv_lines(&mcp.env, '=');
    let headers_init = format_kv_lines(&mcp.headers, ':');
    Some(McpDraftGroup {
        cwd_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:mcp-cwd"),
            "作業ディレクトリ (cwd)",
            Some(&mcp.cwd),
            theme,
        ),
        secret_env_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:mcp-secret-env"),
            "例: GITHUB_TOKEN",
            Some(&mcp.secret_env_var),
            theme,
        ),
        auth_hdr_d: pane.drafts.draft(
            cx,
            format!("conn:{id}:r{rev}:mcp-auth-header"),
            "例: Authorization / X-Api-Key",
            Some(&mcp.auth_header),
            theme,
        ),
        env_d: pane.drafts.description_draft(
            cx,
            format!("conn:{id}:r{rev}:mcp-env"),
            "KEY=VALUE（1行に1つ）",
            Some(&env_init),
            theme,
        ),
        headers_d: pane.drafts.description_draft(
            cx,
            format!("conn:{id}:r{rev}:mcp-headers"),
            "Header-Name: value（1行に1つ）",
            Some(&headers_init),
            theme,
        ),
    })
}

pub(super) fn render_mcp_tools_strip(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    list_d: &Entity<Draft>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<gpui::Div> {
    if conn.kind != ConnectionKind::Mcp {
        return None;
    }
    let caps = check.map(|c| &c.resources).filter(|c| !c.is_empty())?;
    let id = &conn.id;
    let filter_d = pane.drafts.draft(
        cx,
        format!("conn:{id}:mcp-filter"),
        "ツール名で絞り込み…",
        None,
        theme,
    );
    let filter = filter_d.read(cx).text().trim().to_lowercase();
    let current_allowed = split_comma(list_d.read(cx).text());
    let chips: Vec<AnyElement> = caps
        .iter()
        .filter(|cap| filter.is_empty() || cap.to_lowercase().contains(&filter))
        .map(|cap| {
            let selected = current_allowed.iter().any(|a| a == cap);
            let ld = list_d.clone();
            let next_list = toggle_comma_item(&current_allowed, cap);
            pill_button(
                SharedString::from(format!("mcp-tool-{id}-{cap}")),
                cap.clone(),
                if selected { Tone::Primary } else { Tone::Ghost }
                    .maybe_icon(selected.then_some(Icon::Check)),
                theme,
                move |_, cx| ld.update(cx, |d, cx| d.set(next_list.clone(), cx)),
            )
        })
        .collect();
    Some(
        div()
            .rounded(crate::ui::theme::radius::medium())
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
}

pub(super) fn read_mcp_extra(
    group: &McpDraftGroup,
    cx: &gpui::App,
) -> Result<McpExtraConfig, &'static str> {
    let env = parse_mcp_kv(group.env_d.read(cx).text(), '=').map_err(|error| error.message('='))?;
    let headers =
        parse_mcp_kv(group.headers_d.read(cx).text(), ':').map_err(|error| error.message(':'))?;
    Ok(McpExtraConfig {
        cwd: group.cwd_d.read(cx).text().trim().to_owned(),
        secret_env_var: group.secret_env_d.read(cx).text().trim().to_owned(),
        auth_header: group.auth_hdr_d.read(cx).text().trim().to_owned(),
        env,
        headers,
    })
}

pub(super) fn append_mcp_extra_fields(
    panel: gpui::Div,
    id: &str,
    group: McpDraftGroup,
    mcp_kv_error: Option<&'static str>,
    theme: &Theme,
) -> gpui::Div {
    panel
        .child(labeled_field("作業ディレクトリ (cwd)", group.cwd_d, theme))
        .child(labeled_field(
            "認証シークレット環境変数名 (stdio)",
            group.secret_env_d,
            theme,
        ))
        .child(labeled_field(
            "認証ヘッダー名 (HTTP・空欄時は Bearer)",
            group.auth_hdr_d,
            theme,
        ))
        .child(labeled_field(
            "環境変数（KEY=VALUE を1行ずつ）",
            div()
                .id(SharedString::from(format!("conn-mcp-env-{id}")))
                .max_h(px(110.))
                .overflow_y_scroll()
                .child(group.env_d),
            theme,
        ))
        .child(labeled_field(
            "HTTP ヘッダー（Name: Value を1行ずつ）",
            div()
                .id(SharedString::from(format!("conn-mcp-headers-{id}")))
                .max_h(px(110.))
                .overflow_y_scroll()
                .child(group.headers_d),
            theme,
        ))
        .when_some(mcp_kv_error, |this, error| {
            this.child(div().text_xs().text_color(theme.danger_fg).child(error))
        })
}
