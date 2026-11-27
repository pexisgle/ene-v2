//! Connection rows and the inline editor.
pub(super) mod form;
pub(super) mod views;

use super::ConnectionsPane;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, kind_pill, pill_button, status_pill};
use ene_client_core::connections::ConnectionResource;
use ene_client_core::domain::{Connection, ConnectionKind, LlmProvider};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

use form::*;
use views::*;

pub(super) fn render_connection_row(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let id = conn.id.clone();
    let expanded = pane.form.editor(&id).is_some();
    let providers = pane
        .hub
        .read(cx)
        .model
        .connections
        .providers
        .as_deref()
        .unwrap_or_default();
    let needs_key_flag = conn.needs_key(providers);
    let disclosure = speech_disclosure(conn, providers);
    let check = conn.last_check.as_ref();
    let badge = compute_connection_badge(conn, check, needs_key_flag, theme);
    let header = render_row_header(
        pane,
        conn,
        &badge,
        &RowHeaderFlags { expanded, live },
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
        .when_some(disclosure, |row, text| {
            row.child(
                div()
                    .px(px(12.))
                    .pb(px(10.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(text),
            )
        })
        .when_some(detail, gpui::ParentElement::child)
        .into_any_element()
}

fn speech_disclosure(connection: &Connection, providers: &[LlmProvider]) -> Option<String> {
    let sample = match connection.kind {
        ConnectionKind::Tts => "テスト文「接続確認です。」",
        ConnectionKind::Stt => "無音のテスト音声（0.5秒）",
        _ => return None,
    };
    let endpoint = connection.endpoint()?;
    let mut destination = endpoint.endpoint.trim();
    if destination.is_empty() {
        destination = if connection.is_local_voicevox() {
            ene_client_core::VOICEVOX_DEFAULT_ENDPOINT
        } else {
            providers
                .iter()
                .find(|provider| provider.adapter == endpoint.adapter)?
                .endpoint
                .as_str()
        };
    }
    Some(format!(
        "接続確認では {destination} に{sample}を送信します。"
    ))
}

fn render_row_header(
    pane: &ConnectionsPane,
    conn: &Connection,
    badge: &RowBadge,
    flags: &RowHeaderFlags,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let RowHeaderFlags { expanded, live } = *flags;
    let id = &conn.id;
    let kind = conn.kind;
    let hub_check = pane.hub.clone();
    let revision = hub_check
        .read(cx)
        .model
        .settings()
        .map(|settings| settings.revision);
    let is_checking = hub_check
        .read(cx)
        .setup
        .checking_connections
        .contains_key(id);
    let is_voicevox = ConnectionResource::for_connection(conn) == Some(ConnectionResource::Speaker);
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
                .when(kind != ConnectionKind::Llm, |row| {
                    row.child(pill_button(
                        SharedString::from(format!("conn-check-{id}")),
                        if is_checking && is_voicevox {
                            "起動・確認中…"
                        } else if is_checking {
                            "確認中…"
                        } else {
                            "確認"
                        },
                        Tone::Ghost
                            .icon(Icon::RefreshCw)
                            .enabled(live && !is_checking),
                        theme,
                        move |_, cx| {
                            if let Some(revision) = revision {
                                hub_check.update(cx, |hub, cx| {
                                    hub.check_connection_at(check_id.clone(), revision, None, cx)
                                });
                            }
                        },
                    ))
                })
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
                        pane_expand.update(cx, |this, cx| {
                            if this.form.editor(&expand_id).is_none() {
                                this.form.open_editor(expand_id.clone());
                            } else {
                                this.form.close_editor();
                            }
                            cx.notify();
                        });
                    },
                )),
        )
}
