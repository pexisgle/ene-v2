use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{CircleButtonStyle, Tone, circle_icon_button, pill_button, status_pill};
use crate::ui::{Hub, SettingsStudio, search_adapter_requires_key};
use ene_client_core::domain::{Connection, ConnectionCheck, ConnectionKind};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};
use std::collections::BTreeMap;

struct SearchProviderSpec {
    adapter: &'static str,
    display_name: &'static str,
    placeholder: &'static str,
    env_hint: &'static str,
}

const SEARCH_PROVIDERS: [SearchProviderSpec; 2] = [
    SearchProviderSpec {
        adapter: "exa",
        display_name: "Exa",
        placeholder: "Exa API キーを入力…",
        env_hint: "EXA_API_KEY",
    },
    SearchProviderSpec {
        adapter: "tavily",
        display_name: "Tavily",
        placeholder: "Tavily API キーを入力… (tvly-...)",
        env_hint: "TAVILY_API_KEY",
    },
];

impl SettingsStudio {
    pub(super) fn render_web_search_card(
        &mut self,
        connections: &[Connection],
        checks: &BTreeMap<String, ConnectionCheck>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let search_conns: Vec<&Connection> = connections
            .iter()
            .filter(|c| c.kind == ConnectionKind::Search)
            .collect();
        let active_conn = search_conns.iter().copied().find(|c| {
            c.enabled
                && (!c.search_adapter().is_some_and(search_adapter_requires_key)
                    || matches!(c.credential_state.as_str(), "registered" | "environment"))
        });
        let active_fallback =
            active_conn.or_else(|| search_conns.iter().copied().find(|c| c.enabled));

        let provider_rows: Vec<AnyElement> = SEARCH_PROVIDERS
            .iter()
            .map(|spec| {
                self.render_search_provider_row(spec, &search_conns, (checks, live), theme, cx)
            })
            .collect();
        let switcher =
            render_search_switcher(&search_conns, active_fallback, live, &self.hub, theme);

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
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
            .when_some(switcher, |this, sw| this.child(sw))
            .children(provider_rows)
            .into_any_element()
    }

    fn render_search_provider_row(
        &mut self,
        spec: &SearchProviderSpec,
        search_conns: &[&Connection],
        (checks, live): (&BTreeMap<String, ConnectionCheck>, bool),
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let existing = search_conns.iter().copied().find(|c| {
            c.search_adapter()
                .is_some_and(|a| a.eq_ignore_ascii_case(spec.adapter))
        });
        let conn_id = existing.map(|c| c.id.clone());
        let has_cred = existing
            .is_some_and(|c| matches!(c.credential_state.as_str(), "registered" | "environment"));
        let is_active = existing.is_some_and(|c| c.enabled);
        let is_checking = conn_id
            .as_ref()
            .is_some_and(|id| self.hub.read(cx).checking_connections.contains(id));
        let check = existing.and_then(|c| checks.get(&c.id).or(c.last_check.as_ref()));

        let reveal_key = format!("search-card:{}", spec.adapter);
        let revealed =
            self.connection_form.revealed_secret_for.as_deref() == Some(reveal_key.as_str());
        let draft = self.masked_draft(
            cx,
            format!("search-card:secret:{}", spec.adapter),
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
            &self.hub,
            theme,
        );
        let input_row = render_provider_key_row(
            spec.adapter,
            ProviderKeyInput {
                draft,
                reveal_key,
                revealed,
                has_cred,
                key_text,
                live,
            },
            &self.hub,
            theme,
            cx,
        );

        div()
            .w_full()
            .rounded(px(10.))
            .bg(theme.bg_blush)
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(header)
            .child(input_row)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(format!("環境変数 {} でも設定できます", spec.env_hint)),
            )
            .when_some(err_msg, |this, msg| {
                this.child(
                    div()
                        .rounded(px(8.))
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
    let (cred_label, cred_bg, cred_fg) = match meta.existing.map(|c| c.credential_state.as_str()) {
        Some("registered") => ("キー登録済み", theme.mint_soft, theme.mint_fg),
        Some("environment") => ("環境変数", theme.mint_soft, theme.mint_fg),
        _ => ("未登録", theme.bg_surface, theme.ink_muted),
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
                        .child(spec.display_name),
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
                            SharedString::from(format!("search-card-use-{}", spec.adapter)),
                            "この検索を使う",
                            Tone::Soft.icon(Icon::Check).enabled(meta.live),
                            theme,
                            move |_, cx| {
                                hub_use.update(cx, |hub, cx| {
                                    hub.assign_search_connection(Some(cid.clone()), cx);
                                });
                            },
                        ))
                    },
                )
                .when_some(check_id.filter(|_| meta.has_cred), |this, cid| {
                    this.child(pill_button(
                        SharedString::from(format!("search-card-check-{}", spec.adapter)),
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

struct ProviderKeyInput {
    draft: Entity<crate::input::Draft>,
    reveal_key: String,
    revealed: bool,
    has_cred: bool,
    key_text: String,
    live: bool,
}

fn render_provider_key_row(
    adapter: &'static str,
    input: ProviderKeyInput,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let studio_eye = cx.entity();
    let hub_save = hub.clone();
    let revealed = input.revealed;
    let reveal_key = input.reveal_key;
    let key_text = input.key_text;
    let can_save = input.live && !key_text.is_empty();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(div().flex_1().min_w_0().child(input.draft))
        .child(circle_icon_button(
            SharedString::from(format!("search-card-eye-{adapter}")),
            CircleButtonStyle::new(if revealed { Icon::EyeOff } else { Icon::Eye }, 30., 14.),
            theme,
            move |_, cx| {
                studio_eye.update(cx, |this, cx| {
                    this.connection_form.revealed_secret_for = if revealed {
                        None
                    } else {
                        Some(reveal_key.clone())
                    };
                    cx.notify();
                });
            },
        ))
        .child(pill_button(
            SharedString::from(format!("search-card-save-{adapter}")),
            if input.has_cred {
                "キーを更新"
            } else {
                "登録して使う"
            },
            Tone::Primary.icon(Icon::Check).enabled(can_save),
            theme,
            move |_, cx| {
                hub_save.update(cx, |hub, cx| {
                    hub.save_search_provider_key(adapter, key_text.clone(), true, cx);
                });
            },
        ))
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
                    hub_sw.update(cx, |hub, cx| {
                        hub.assign_search_connection(Some(cid.clone()), cx)
                    });
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
