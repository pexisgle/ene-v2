//! Connections chapter in `SettingsStudio`: single-line 62px rows with status pills,
//! 1-level hairline expansion, dynamic `options_schema`, MCP tool selector,
//! and bottom collapsed disclosure for model-role & voice slot overrides.

mod add_panel;
mod mcp_editor;
mod pickers;
mod roles_panel;
mod row_editor;
mod schema_form;
mod web_search;

use crate::ui::GOOGLE_AGENT_PLATFORM;
use crate::ui::SettingsStudio;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, disclosure_row, pill_button};
use ene_client_core::domain::{Connection, ConnectionCheck, ConnectionKind};
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};
use std::collections::BTreeMap;

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
                hub.model.connections.providers.clone().unwrap_or_default(),
                hub.model.connections.checks.clone(),
                hub.model.companion().cloned(),
                hub.all_llm_models(),
                hub.all_embedding_models(),
            )
        };

        self.consume_pending_mcp_probe(&connections, cx);
        let adding = self.connection_form.adding_connection;
        let add_panel =
            adding.then(|| self.render_add_connection_panel(live, &providers, theme, cx));
        let search_card = self.render_web_search_card(&connections, &checks, live, theme, cx);
        let rows = self.build_connection_rows(&connections, (&checks, live, adding), theme, cx);
        let adv_open = self.connection_form.advanced_roles_open;
        let adv_panel = adv_open.then(|| {
            self.render_advanced_roles_panel(
                companion.as_ref(),
                super::connections::roles_panel::ModelRoleChoices {
                    all_models: &all_models,
                    embedding_models: &embedding_models,
                },
                live,
                theme,
                cx,
            )
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(render_connections_header(adding, theme, cx))
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
                    .child(render_advanced_roles_toggle(adv_open, theme, cx))
                    .when_some(adv_panel, |this, p| this.child(p)),
            )
            .into_any_element()
    }

    fn consume_pending_mcp_probe(&mut self, connections: &[Connection], cx: &mut Context<Self>) {
        if self
            .connection_form
            .pending_mcp
            .as_ref()
            .is_some_and(|id| connections.iter().any(|connection| &connection.id == id))
            && let Some(id) = self.connection_form.pending_mcp.take()
        {
            self.clear_mcp_drafts();
            let hub = self.hub.clone();
            cx.defer(move |cx| hub.update(cx, |hub, cx| hub.check_connection(id, cx)));
        }
    }

    fn build_connection_rows(
        &mut self,
        connections: &[Connection],
        (checks, live, adding): (&BTreeMap<String, ConnectionCheck>, bool, bool),
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = connections
            .iter()
            .map(|conn| {
                let check = effective_check(conn, checks);
                self.render_connection_row(conn, check, live, theme, cx)
            })
            .collect();
        if rows.is_empty() && !adding {
            rows.push(render_empty_connections_notice(theme));
        }
        rows
    }
}

fn effective_check<'a>(
    conn: &'a Connection,
    checks: &'a BTreeMap<String, ConnectionCheck>,
) -> Option<&'a ConnectionCheck> {
    if conn
        .llm_endpoint()
        .is_some_and(|ep| ep.adapter == GOOGLE_AGENT_PLATFORM)
    {
        conn.last_check.as_ref()
    } else {
        checks.get(&conn.id).or(conn.last_check.as_ref())
    }
}

fn render_connections_header(
    adding: bool,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let studio_add = cx.entity();
    let studio_mcp = cx.entity();
    div()
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
                    Tone::Ghost.icon(Icon::Plus),
                    theme,
                    move |_, cx| {
                        studio_mcp.update(cx, |this, cx| {
                            this.connection_form.adding_connection = true;
                            this.connection_form.custom_connection_open = true;
                            this.connection_form.custom_kind = ConnectionKind::Mcp;
                            this.connection_form.mcp_add_mode = crate::ui::mcp_add::Mode::Npx;
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
                    if adding { Tone::Ghost } else { Tone::Primary }
                        .maybe_icon((!adding).then_some(Icon::Plus)),
                    theme,
                    move |_, cx| {
                        studio_add.update(cx, |this, cx| {
                            this.connection_form.adding_connection =
                                !this.connection_form.adding_connection;
                            if this.connection_form.adding_connection
                                && this.connection_form.custom_kind == ConnectionKind::Mcp
                            {
                                this.connection_form.custom_kind = ConnectionKind::Llm;
                            }
                            cx.notify();
                        });
                    },
                )),
        )
}

fn render_empty_connections_notice(theme: &Theme) -> AnyElement {
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
        .into_any_element()
}

fn render_advanced_roles_toggle(
    adv_open: bool,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> AnyElement {
    let studio_adv = cx.entity();
    disclosure_row(
        "conn-advanced-roles-toggle",
        "詳細設定（役割別のモデル割当・エンドポイント）",
        adv_open,
        theme,
        move |_, cx| {
            studio_adv.update(cx, |this, cx| {
                this.connection_form.advanced_roles_open =
                    !this.connection_form.advanced_roles_open;
                cx.notify();
            });
        },
    )
}
