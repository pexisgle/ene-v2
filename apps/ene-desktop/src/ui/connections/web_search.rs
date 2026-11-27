use super::{ConnectionsPane, add_panel::build_search_preset_chips};
use crate::ui::hub::presets;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, status_pill};
use crate::ui::{Hub, search_adapter_requires_key};
use ene_client_core::domain::{Connection, ConnectionKind};
use ene_domain::SearchSelection;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_web_search_card(
    pane: &ConnectionsPane,
    connections: &[Connection],
    live: bool,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> AnyElement {
    let search: Vec<_> = connections
        .iter()
        .filter(|c| c.kind == ConnectionKind::Search)
        .collect();
    let selection = SearchSelection::from_enabled(
        search
            .iter()
            .copied()
            .filter(|connection| connection.enabled),
    );

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
        .child(render_search_card_header(selection, theme))
        .when_some(
            render_search_switcher(&search, selection, live, &pane.hub, theme),
            gpui::ParentElement::child,
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(build_search_preset_chips(live, &pane.hub, theme, cx)),
        )
        .into_any_element()
}

fn render_search_card_header(selection: SearchSelection<&Connection>, theme: &Theme) -> gpui::Div {
    let (summary_label, summary_dot, summary_bg, summary_fg) = match selection {
        SearchSelection::Selected(conn)
            if conn
                .search_adapter()
                .is_some_and(search_adapter_requires_key)
                && !conn.credential_state.is_configured() =>
        {
            (
                format!("{}（キー登録が必要）", conn.name),
                theme.amber,
                theme.amber_soft,
                theme.amber_fg,
            )
        }
        SearchSelection::Selected(conn) => (
            format!("選択中 {}", conn.name),
            theme.mint,
            theme.mint_soft,
            theme.mint_fg,
        ),
        SearchSelection::Ambiguous => (
            "検索先を1件選択してください".into(),
            theme.amber,
            theme.amber_soft,
            theme.amber_fg,
        ),
        SearchSelection::Off => (
            "検索オフ".into(),
            theme.ink_placeholder,
            theme.bg_blush,
            theme.ink_muted,
        ),
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
                    .child("Web 検索"),
            ),
        )
        .child(status_pill(
            summary_label,
            Some(summary_dot),
            summary_bg,
            summary_fg,
        ))
}

fn render_search_switcher(
    search_conns: &[&Connection],
    selection: SearchSelection<&Connection>,
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
            let selected =
                matches!(selection, SearchSelection::Selected(active) if active.id == conn.id);
            let label = if search_conns
                .iter()
                .any(|other| other.id != conn.id && other.name == conn.name)
            {
                format!("{} · {}", conn.name, conn.id)
            } else {
                conn.name.clone()
            };
            let hub_sw = hub.clone();
            pill_button(
                SharedString::from(format!("search-switch-{}", conn.id)),
                label,
                if selected { Tone::Primary } else { Tone::Ghost }
                    .maybe_icon(selected.then_some(Icon::Check))
                    .enabled(live),
                theme,
                move |_, cx| {
                    hub_sw.update(cx, |hub, cx| {
                        presets::assign_search_connection(hub, Some(&cid), cx)
                    });
                },
            )
        })
        .collect();
    let hub_off = hub.clone();
    let is_off = matches!(selection, SearchSelection::Off);
    chips.push(pill_button(
        "search-switch-off",
        "オフ",
        if is_off { Tone::Soft } else { Tone::Ghost }
            .maybe_icon(is_off.then_some(Icon::Check))
            .enabled(live),
        theme,
        move |_, cx| {
            hub_off.update(cx, |hub, cx| {
                presets::assign_search_connection(hub, None, cx)
            })
        },
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
