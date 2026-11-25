//! Connection rows and the inline editor.
pub(super) mod form;
pub(super) mod views;

use super::ConnectionsPane;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, kind_pill, pill_button, status_pill};
use ene_client_core::domain::{Connection, ConnectionCheck};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

pub(crate) use form::is_voicevox_tts;
use form::*;
use views::*;

pub(super) fn render_connection_row(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let id = conn.id.clone();
    let expanded = pane.form.expanded_connection_id.as_deref() == Some(id.as_str());
    let needs_key_flag = conn.needs_key(
        pane.hub
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
        pane,
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
    let detail = expanded.then(|| render_connection_editor(pane, conn, check, live, theme, cx));

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .flex()
        .flex_col()
        .child(header)
        .when_some(detail, gpui::ParentElement::child)
        .into_any_element()
}

fn render_row_header(
    pane: &ConnectionsPane,
    conn: &Connection,
    badge: &RowBadge,
    flags: RowHeaderFlags,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let RowHeaderFlags {
        expanded,
        should_auto_probe,
        live,
    } = flags;
    let id = &conn.id;
    let kind = conn.kind;
    let hub_check = pane.hub.clone();
    let is_checking = hub_check.read(cx).setup.checking_connections.contains(id);
    let ep = conn.endpoint();
    let is_voicevox = is_voicevox_tts(conn, ep);
    let hub_expand_probe = hub_check.clone();
    let check_id = id.clone();
    let expand_id = id.clone();
    let pane_expand = cx.entity();

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
                        pane_expand.update(cx, |this, cx| {
                            if this.form.expanded_connection_id.as_deref()
                                == Some(expand_id.as_str())
                            {
                                this.form.expanded_connection_id = None;
                            } else {
                                this.form.expanded_connection_id = Some(expand_id.clone());
                                if should_auto_probe
                                    && this.form.auto_probed_connections.insert(expand_id.clone())
                                {
                                    probe_id = Some(expand_id.clone());
                                }
                            }
                            this.open_dropdown = None;
                            this.form.revoke_armed_for = None;
                            cx.notify();
                        });
                        if let Some(pid) = probe_id {
                            hub_expand_probe.update(cx, |hub, cx| hub.check_connection(pid, cx));
                        }
                    },
                )),
        )
}
