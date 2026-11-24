//! Connections chapter in `SettingsStudio`: single-line 62px rows with status pills,
//! 1-level hairline expansion, dynamic `options_schema`, MCP tool selector,
//! and bottom collapsed disclosure for model-role & voice slot overrides.

mod add_panel;
mod kind_pickers;
pub(crate) mod mcp_add;
mod mcp_editor;
mod pickers;
mod roles_panel;
mod roles_voice_search;
mod row_editor;
mod schema_form;
mod web_search;

use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, disclosure_row, pill_button};
use crate::ui::{DraftStore, GOOGLE_AGENT_PLATFORM, Hub};
use ene_client_core::domain::{Connection, ConnectionCheck, ConnectionKind};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px};
use std::collections::{BTreeMap, HashSet};

/// Form and disclosure state owned by [`ConnectionsPane`].
pub(super) struct ConnectionFormState {
    pub(super) adding_connection: bool,
    pub(super) custom_connection_open: bool,
    pub(super) custom_kind: ConnectionKind,
    pub(super) mcp_add_mode: mcp_add::Mode,
    pub(super) mcp_add_advanced_open: bool,
    pub(super) pending_mcp: Option<String>,
    pub(super) expanded_connection_id: Option<String>,
    pub(super) revealed_secret_for: Option<String>,
    pub(super) revoke_armed_for: Option<String>,
    pub(super) advanced_roles_open: bool,
    pub(super) auto_probed_connections: HashSet<String>,
    last_cleared_secret_seq: u64,
}

impl Default for ConnectionFormState {
    fn default() -> Self {
        Self {
            adding_connection: false,
            custom_connection_open: false,
            custom_kind: ConnectionKind::Llm,
            mcp_add_mode: mcp_add::Mode::Npx,
            mcp_add_advanced_open: false,
            pending_mcp: None,
            expanded_connection_id: None,
            revealed_secret_for: None,
            revoke_armed_for: None,
            advanced_roles_open: false,
            auto_probed_connections: HashSet::new(),
            last_cleared_secret_seq: 0,
        }
    }
}

/// Connections chapter rendered as an independent GPUI pane entity.
pub(crate) struct ConnectionsPane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    pub(super) form: ConnectionFormState,
    pub(super) open_dropdown: Option<String>,
    initialized: bool,
    _observe: Subscription,
}

impl std::fmt::Debug for ConnectionsPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionsPane")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

impl ConnectionsPane {
    pub(crate) fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            drafts: DraftStore::default(),
            form: ConnectionFormState::default(),
            open_dropdown: None,
            initialized: false,
            _observe,
        }
    }

    pub(crate) fn on_leave(&mut self) {
        self.open_dropdown = None;
    }

    pub(super) fn clear_mcp_drafts(&mut self) {
        self.drafts.retain(|key| !key.starts_with("add:mcp:"));
    }

    fn sync_hub_events(&mut self, cx: &mut Context<Self>) {
        let cleared = self.hub.read(cx).setup.cleared_secret_for.clone();
        if !self.initialized {
            self.initialized = true;
            self.form.last_cleared_secret_seq = cleared.as_ref().map_or(0, |(_, seq)| *seq);
            return;
        }
        if let Some((conn_id, seq)) = cleared
            && seq != self.form.last_cleared_secret_seq
        {
            self.form.last_cleared_secret_seq = seq;
            let adapter = self.hub.read(cx).model.settings().and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == conn_id)
                    .and_then(|c| c.search_adapter())
                    .map(|a| a.to_ascii_lowercase())
            });
            let mut keys = vec![format!("conn:secret:{conn_id}")];
            if let Some(adapter) = adapter {
                keys.push(format!("search-card:secret:{adapter}"));
            } else if let Some(suffix) = conn_id.strip_prefix("search-") {
                keys.push(format!("search-card:secret:{suffix}"));
            }
            self.drafts.clear_keys(cx, keys.iter().map(String::as_str));
            if self.form.revealed_secret_for.as_deref() == Some(conn_id.as_str())
                || self
                    .form
                    .revealed_secret_for
                    .as_deref()
                    .is_some_and(|r| r.starts_with("search-card:"))
            {
                self.form.revealed_secret_for = None;
            }
        }
    }

    fn consume_pending_mcp_probe(&mut self, connections: &[Connection], cx: &mut Context<Self>) {
        if self
            .form
            .pending_mcp
            .as_ref()
            .is_some_and(|id| connections.iter().any(|connection| &connection.id == id))
            && let Some(id) = self.form.pending_mcp.take()
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
                row_editor::render_connection_row(self, conn, check, live, theme, cx)
            })
            .collect();
        if rows.is_empty() && !adding {
            rows.push(render_empty_connections_notice(theme));
        }
        rows
    }
}

impl gpui::Render for ConnectionsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_hub_events(cx);
        let (theme, live, connections, providers, checks, companion, all_models, embedding_models) = {
            let hub = self.hub.read(cx);
            (
                hub.theme(),
                hub.live(),
                hub.model
                    .settings()
                    .map_or_default(|s| s.connections.clone()),
                hub.model.connections.providers.clone().unwrap_or_default(),
                hub.model.connections.checks.clone(),
                hub.model.companion().cloned(),
                hub.all_llm_models(),
                hub.all_embedding_models(),
            )
        };

        self.consume_pending_mcp_probe(&connections, cx);
        let adding = self.form.adding_connection;
        let add_panel = adding
            .then(|| add_panel::render_add_connection_panel(self, live, &providers, &theme, cx));
        let search_card =
            web_search::render_web_search_card(self, &connections, &checks, live, &theme, cx);
        let rows = self.build_connection_rows(&connections, (&checks, live, adding), &theme, cx);
        let adv_open = self.form.advanced_roles_open;
        let adv_panel = adv_open.then(|| {
            roles_panel::render_advanced_roles_panel(
                self,
                companion.as_ref(),
                roles_panel::ModelRoleChoices {
                    all_models: &all_models,
                    embedding_models: &embedding_models,
                },
                live,
                &theme,
                cx,
            )
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(render_connections_header(adding, &theme, cx))
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
                    .child(render_advanced_roles_toggle(adv_open, &theme, cx))
                    .when_some(adv_panel, |this, p| this.child(p)),
            )
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
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let pane_add = cx.entity();
    let pane_mcp = cx.entity();
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
                        pane_mcp.update(cx, |this, cx| {
                            this.form.adding_connection = true;
                            this.form.custom_connection_open = true;
                            this.form.custom_kind = ConnectionKind::Mcp;
                            this.form.mcp_add_mode = mcp_add::Mode::Npx;
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
                        pane_add.update(cx, |this, cx| {
                            this.form.adding_connection = !this.form.adding_connection;
                            if this.form.adding_connection
                                && this.form.custom_kind == ConnectionKind::Mcp
                            {
                                this.form.custom_kind = ConnectionKind::Llm;
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
        .rounded(crate::ui::theme::radius::surface())
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
    cx: &Context<ConnectionsPane>,
) -> AnyElement {
    let pane_adv = cx.entity();
    disclosure_row(
        "conn-advanced-roles-toggle",
        "詳細設定（役割別のモデル割当・エンドポイント）",
        adv_open,
        theme,
        move |_, cx| {
            pane_adv.update(cx, |this, cx| {
                this.form.advanced_roles_open = !this.form.advanced_roles_open;
                cx.notify();
            });
        },
    )
}
