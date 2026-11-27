use super::{
    ConnectionsPane,
    mcp_fields::{FieldMode, McpFields},
};
use crate::input::Draft;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button};
use ene_client_core::{
    connections::{split_comma, toggle_comma_item},
    domain::{Connection, ConnectionCheck, ConnectionConfig, ConnectionKind},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

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
) -> Option<McpFields> {
    let Some(ConnectionConfig::Mcp(mcp)) = cfg else {
        return None;
    };
    let prefix = format!("conn:{id}:r{rev}:mcp-");
    Some(McpFields::new(
        &mut pane.drafts,
        &prefix,
        FieldMode::Both,
        Some(mcp),
        theme,
        cx,
    ))
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

pub(super) fn append_mcp_extra_fields(
    panel: gpui::Div,
    id: &str,
    group: McpFields,
    mcp_kv_error: Option<&'static str>,
    theme: &Theme,
) -> gpui::Div {
    group
        .append(panel, None, Some(id), theme)
        .when_some(mcp_kv_error, |this, error| {
            this.child(div().text_xs().text_color(theme.danger_fg).child(error))
        })
}
