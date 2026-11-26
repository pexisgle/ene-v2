//! Connections chapter in `SettingsStudio`: single-line 62px rows with status pills,
//! 1-level hairline expansion, dynamic `options_schema`, MCP tool selector,
//! and bottom collapsed disclosure for model-role & voice slot overrides.

mod add_panel;
pub(crate) mod mcp_add;
mod mcp_editor;
mod pickers;
mod roles_panel;
mod roles_voice_search;
mod row_editor;
mod schema_form;
mod web_search;

use crate::input::Draft;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    CircleButtonStyle, Tone, circle_icon_button, disclosure_row, pill_button,
};
use crate::ui::{DraftStore, GOOGLE_AGENT_PLATFORM, Hub};
use ene_client_core::domain::{Connection, ConnectionCheck, ConnectionKind};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Default)]
enum AddFlow {
    #[default]
    Closed,
    Presets,
    Custom(ConnectionKind),
    Mcp(mcp_add::Form),
}

#[derive(Clone, PartialEq, Eq)]
enum SecretField {
    Connection(String),
    Search(&'static str),
}

struct ExpandedConnection {
    id: String,
    revoke_armed: bool,
}

#[derive(Default)]
struct ConnectionFormState {
    add: AddFlow,
    pending_mcp: Option<String>,
    expanded: Option<ExpandedConnection>,
    revealed: Option<SecretField>,
    advanced_roles_open: bool,
    auto_probed_connections: HashSet<String>,
    last_cleared_secret_seq: u64,
}

impl ConnectionFormState {
    fn editor(&self, id: &str) -> Option<&ExpandedConnection> {
        self.expanded.as_ref().filter(|editor| editor.id == id)
    }

    fn open_editor(&mut self, id: String) {
        self.close_editor();
        self.expanded = Some(ExpandedConnection {
            id,
            revoke_armed: false,
        });
    }

    fn close_editor(&mut self) {
        self.expanded = None;
        if matches!(self.revealed, Some(SecretField::Connection(_))) {
            self.revealed = None;
        }
    }
}

fn secret_row(
    button_id: String,
    field: SecretField,
    draft: Entity<Draft>,
    revealed: bool,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let pane = cx.entity();
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().flex_1().min_w_0().child(draft))
        .child(circle_icon_button(
            gpui::SharedString::from(button_id),
            CircleButtonStyle::new(if revealed { Icon::EyeOff } else { Icon::Eye }, 30., 14.),
            theme,
            move |_, cx| {
                pane.update(cx, |this, cx| {
                    if let SecretField::Connection(id) = &field
                        && this.form.editor(id).is_none()
                    {
                        return;
                    }
                    this.form.revealed =
                        (this.form.revealed.as_ref() != Some(&field)).then(|| field.clone());
                    cx.notify();
                });
            },
        ))
}

/// Connections chapter rendered as an independent GPUI pane entity.
pub(crate) struct ConnectionsPane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    form: ConnectionFormState,
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

fn toggle_pane_dropdown(pane: &Entity<ConnectionsPane>, key: &str, cx: &mut gpui::App) {
    pane.update(cx, |this, cx| {
        this.open_dropdown = if this.open_dropdown.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_owned())
        };
        cx.notify();
    });
}

fn close_pane_dropdown(pane: &Entity<ConnectionsPane>, cx: &mut gpui::App) {
    pane.update(cx, |this, cx| {
        this.open_dropdown = None;
        cx.notify();
    });
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
        self.form.revealed = None;
        if let Some(editor) = &mut self.form.expanded {
            editor.revoke_armed = false;
        }
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
                    .map(str::to_ascii_lowercase)
            });
            let search_adapter = adapter
                .as_deref()
                .or_else(|| conn_id.strip_prefix("search-"));
            let keys = [
                Some(format!("conn:secret:{conn_id}")),
                search_adapter.map(|adapter| format!("search-card:secret:{adapter}")),
            ];
            self.drafts
                .clear_keys(cx, keys.iter().flatten().map(String::as_str));
            if self
                .form
                .revealed
                .as_ref()
                .is_some_and(|field| match field {
                    SecretField::Connection(id) => id == &conn_id,
                    SecretField::Search(adapter) => Some(*adapter) == search_adapter,
                })
            {
                self.form.revealed = None;
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
        let adding = !matches!(self.form.add, AddFlow::Closed);
        let add_panel = match self.form.add {
            AddFlow::Closed => None,
            AddFlow::Presets | AddFlow::Custom(_) => Some(add_panel::render_add_connection_panel(
                self, live, &providers, &theme, cx,
            )),
            AddFlow::Mcp(form) => Some(mcp_add::render_mcp_add(self, form, live, &theme, cx)),
        };
        let search_card =
            web_search::render_web_search_card(self, &connections, &checks, live, &theme, cx);
        let rows = self.build_connection_rows(&connections, (&checks, live, adding), &theme, cx);
        let adv_open = self.form.advanced_roles_open;
        let adv_panel = adv_open.then(|| {
            roles_panel::render_advanced_roles_panel(
                self,
                companion.as_ref(),
                &roles_panel::ModelRoleChoices {
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
            .when_some(add_panel, gpui::ParentElement::child)
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
                    .when_some(adv_panel, gpui::ParentElement::child),
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
                            this.form.add = AddFlow::Mcp(mcp_add::Form::default());
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
                            this.form.add = if adding {
                                AddFlow::Closed
                            } else {
                                AddFlow::Presets
                            };
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
