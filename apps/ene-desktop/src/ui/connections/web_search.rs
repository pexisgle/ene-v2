use super::{ConnectionsPane, SecretField, secret_row};
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill};
use crate::ui::{Hub, search_adapter_requires_key};
use ene_client_core::{
    SearchAdapter,
    domain::{Connection, ConnectionCheck, ConnectionKind},
};
use ene_domain::CredentialState;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};
use std::collections::BTreeMap;

struct SearchProviderSpec {
    adapter: SearchAdapter,
    placeholder: &'static str,
}

const SEARCH_PROVIDERS: [SearchProviderSpec; 2] = [
    SearchProviderSpec {
        adapter: SearchAdapter::Exa,
        placeholder: "Exa API キーを入力…",
    },
    SearchProviderSpec {
        adapter: SearchAdapter::Tavily,
        placeholder: "Tavily API キーを入力… (tvly-...)",
    },
];

pub(super) fn render_web_search_card(
    pane: &mut ConnectionsPane,
    connections: &[Connection],
    checks: &BTreeMap<String, ConnectionCheck>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let search_conns: Vec<&Connection> = connections
        .iter()
        .filter(|c| c.kind == ConnectionKind::Search)
        .collect();
    let active_conn = search_conns.iter().copied().find(|c| {
        c.enabled
            && (!c.search_adapter().is_some_and(search_adapter_requires_key)
                || c.credential_state.is_configured())
    });
    let active_fallback = active_conn.or_else(|| search_conns.iter().copied().find(|c| c.enabled));

    let provider_rows: Vec<AnyElement> = SEARCH_PROVIDERS
        .iter()
        .map(|spec| {
            render_search_provider_row(pane, spec, &search_conns, (checks, live), theme, cx)
        })
        .collect();
    let switcher = render_search_switcher(&search_conns, active_fallback, live, &pane.hub, theme);

    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(render_search_card_header(active_conn, active_fallback, theme))
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("Exa や Tavily の API キーを登録して使用する検索サービスを選べます（Mwmbl・SearXNG・Brave は「接続を追加」からも設定できます）。"),
        )
        .when_some(switcher, gpui::ParentElement::child)
        .children(provider_rows)
        .into_any_element()
}

fn render_search_provider_row(
    pane: &mut ConnectionsPane,
    spec: &SearchProviderSpec,
    search_conns: &[&Connection],
    (checks, live): (&BTreeMap<String, ConnectionCheck>, bool),
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let provider = spec.adapter.provider();
    let existing = search_conns.iter().copied().find(|c| {
        c.search_adapter()
            .is_some_and(|a| a.eq_ignore_ascii_case(provider.adapter_id))
    });
    let conn_id = existing.map(|c| c.id.clone());
    let has_cred = existing.is_some_and(|c| c.credential_state.is_configured());
    let is_active = existing.is_some_and(|c| c.enabled);
    let is_checking = conn_id
        .as_ref()
        .is_some_and(|id| pane.hub.read(cx).setup.checking_connections.contains(id));
    let check = existing.and_then(|c| checks.get(&c.id).or(c.last_check.as_ref()));

    let field = SecretField::Search(provider.adapter_id);
    let revealed = pane.form.revealed_search == Some(provider.adapter_id);
    let draft = pane.drafts.masked_draft(
        cx,
        format!("search-card:secret:{}", provider.adapter_id),
        spec.placeholder,
        !revealed,
        theme,
    );
    let key_text = draft.read(cx).text().trim().to_owned();
    let err_msg = check
        .filter(|c| !c.healthy)
        .and_then(|c| c.failure.as_ref())
        .map(|f| f.message.trim().to_owned())
        .filter(|m| !m.is_empty());

    let header = render_provider_header(
        spec,
        ProviderRowMeta {
            existing,
            conn_id,
            has_cred,
            is_active,
            is_checking,
            check,
            live,
        },
        &pane.hub,
        theme,
    );
    let adapter = spec.adapter;
    let hub_save = pane.hub.clone();
    let input_row = secret_row(
        format!("search-card-eye-{}", provider.adapter_id),
        field,
        draft,
        revealed,
        theme,
        cx,
    )
    .child(pill_button(
        SharedString::from(format!("search-card-save-{}", provider.adapter_id)),
        if has_cred {
            "キーを更新"
        } else {
            "登録して使う"
        },
        Tone::Primary
            .icon(Icon::Check)
            .enabled(live && !key_text.is_empty()),
        theme,
        move |_, cx| {
            hub_save.update(cx, |hub, cx| {
                hub.save_search_provider_key(adapter, &key_text, true, cx);
            });
        },
    ));

    div()
        .w_full()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(header)
        .child(input_row)
        .when_some(provider.key_environment, |this, environment| {
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(format!("環境変数 {environment} でも設定できます")),
            )
        })
        .when_some(err_msg, |this, msg| {
            this.child(
                div()
                    .rounded(crate::ui::theme::radius::small())
                    .bg(theme.danger_soft)
                    .px(px(10.))
                    .py(px(6.))
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child(msg),
            )
        })
        .into_any_element()
}

struct ProviderRowMeta<'a> {
    existing: Option<&'a Connection>,
    conn_id: Option<String>,
    has_cred: bool,
    is_active: bool,
    is_checking: bool,
    check: Option<&'a ConnectionCheck>,
    live: bool,
}

fn render_search_card_header(
    active_conn: Option<&Connection>,
    active_fallback: Option<&Connection>,
    theme: &Theme,
) -> gpui::Div {
    let (summary_label, summary_dot, summary_bg, summary_fg) = match active_conn {
        Some(conn) => (
            format!("使用中: {}", conn.name),
            theme.mint,
            theme.mint_soft,
            theme.mint_fg,
        ),
        None => match active_fallback {
            Some(conn) => (
                format!("{}（キー登録が必要）", conn.name),
                theme.amber,
                theme.amber_soft,
                theme.amber_fg,
            ),
            None => (
                "未設定（検索オフ）".into(),
                theme.ink_placeholder,
                theme.bg_blush,
                theme.ink_muted,
            ),
        },
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div().flex().flex_row().items_center().gap(px(8.)).child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("Web 検索（Exa / Tavily）"),
            ),
        )
        .child(status_pill(
            summary_label,
            Some(summary_dot),
            summary_bg,
            summary_fg,
        ))
}

fn render_provider_header(
    spec: &SearchProviderSpec,
    meta: ProviderRowMeta<'_>,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let (cred_label, cred_bg, cred_fg) = match meta.existing.map_or_default(|c| c.credential_state)
    {
        CredentialState::Registered => ("キー登録済み", theme.mint_soft, theme.mint_fg),
        CredentialState::Environment => ("環境変数", theme.mint_soft, theme.mint_fg),
        CredentialState::Absent => ("未登録", theme.bg_surface, theme.ink_muted),
    };
    let check_badge = if meta.is_checking {
        Some(("確認中…", theme.amber_soft, theme.amber_fg))
    } else {
        match meta.check.map(|c| c.healthy) {
            Some(true) => Some(("確認済み", theme.mint_soft, theme.mint_fg)),
            Some(false) => Some(("接続エラー", theme.danger_soft, theme.danger_fg)),
            None => None,
        }
    };
    let hub_use = hub.clone();
    let hub_check = hub.clone();
    let use_id = meta.conn_id.clone();
    let check_id = meta.conn_id;

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap(px(6.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child(spec.adapter.provider().name),
                )
                .child(status_pill(cred_label, None, cred_bg, cred_fg))
                .when_some(check_badge, |this, (label, bg, fg)| {
                    this.child(status_pill(label, None, bg, fg))
                })
                .when(meta.is_active && meta.has_cred, |this| {
                    this.child(status_pill(
                        "使用中",
                        Some(theme.mint),
                        theme.mint_soft,
                        theme.mint_fg,
                    ))
                }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .when_some(
                    use_id.filter(|_| meta.has_cred && !meta.is_active),
                    |this, cid| {
                        this.child(pill_button(
                            SharedString::from(format!(
                                "search-card-use-{}",
                                spec.adapter.provider().adapter_id
                            )),
                            "この検索を使う",
                            Tone::Soft.icon(Icon::Check).enabled(meta.live),
                            theme,
                            move |_, cx| {
                                hub_use.update(cx, |hub, cx| {
                                    hub.assign_search_connection(Some(&cid), cx);
                                });
                            },
                        ))
                    },
                )
                .when_some(check_id.filter(|_| meta.has_cred), |this, cid| {
                    this.child(pill_button(
                        SharedString::from(format!(
                            "search-card-check-{}",
                            spec.adapter.provider().adapter_id
                        )),
                        if meta.is_checking {
                            "確認中…"
                        } else {
                            "確認"
                        },
                        Tone::Ghost
                            .icon(Icon::Refresh)
                            .enabled(meta.live && !meta.is_checking),
                        theme,
                        move |_, cx| {
                            hub_check.update(cx, |hub, cx| {
                                hub.check_connection(cid.clone(), cx);
                            });
                        },
                    ))
                }),
        )
}

fn render_search_switcher(
    search_conns: &[&Connection],
    active_fallback: Option<&Connection>,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> Option<gpui::Div> {
    if search_conns.is_empty() {
        return None;
    }
    let mut chips: Vec<AnyElement> = search_conns
        .iter()
        .map(|conn| {
            let cid = conn.id.clone();
            let selected = active_fallback.is_some_and(|a| a.id == conn.id);
            let hub_sw = hub.clone();
            pill_button(
                SharedString::from(format!("search-switch-{}", conn.id)),
                conn.name.clone(),
                if selected { Tone::Primary } else { Tone::Ghost }
                    .maybe_icon(selected.then_some(Icon::Check))
                    .enabled(live),
                theme,
                move |_, cx| {
                    hub_sw.update(cx, |hub, cx| hub.assign_search_connection(Some(&cid), cx));
                },
            )
        })
        .collect();
    let hub_off = hub.clone();
    let is_off = active_fallback.is_none();
    chips.push(pill_button(
        "search-switch-off",
        "オフ",
        if is_off { Tone::Soft } else { Tone::Ghost }
            .maybe_icon(is_off.then_some(Icon::Check))
            .enabled(live),
        theme,
        move |_, cx| hub_off.update(cx, |hub, cx| hub.assign_search_connection(None, cx)),
    ));
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink_muted)
                    .child("使用する検索サービス"),
            )
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
