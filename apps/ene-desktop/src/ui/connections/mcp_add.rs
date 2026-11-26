use super::{AddFlow, ConnectionsPane};
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, disclosure_row, pill_button};
use ene_client_core::{
    connections::{
        McpExtraConfig, McpTargetResult, build_mcp_connection_input, mcp_http_target,
        mcp_local_target, mcp_npx_target, parse_mcp_kv,
    },
    domain::{ConnectionInput, ConnectionKind},
};
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) enum Mode {
    #[default]
    Npx,
    Http,
    Local,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Form {
    mode: Mode,
    advanced: bool,
}

pub(super) fn render_mcp_add(
    pane: &mut ConnectionsPane,
    form: Form,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let mode = form.mode;
    let name = pane
        .drafts
        .draft(cx, "add:mcp:name", "空欄なら接続先から設定", None, theme);
    let (fields, helper, target) = render_mcp_mode_fields(pane, mode, theme, cx);
    let adv_open = form.advanced;
    let (mcp_secret, adv_fields, adv_error, extra_cfg) =
        render_mcp_advanced_section(pane, mode, theme, cx);

    let mut error = target.as_ref().err().copied().or(adv_error);
    let chosen_name = name.read(cx).text().trim();
    if chosen_name.chars().count() > 128 {
        error = Some("名前は128文字以内で入力してください。");
    }
    let connection = build_mcp_connection_input(target, extra_cfg, chosen_name);
    let enabled = live && error.is_none() && connection.is_some();

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
        .child(render_mcp_add_header(theme, cx))
        .child(render_mcp_mode_chips(mode, theme, cx))
        .child(fields)
        .when_some(error.or(helper), |this, text| {
            this.child(
                div()
                    .text_xs()
                    .text_color(if error.is_some() {
                        theme.danger_fg
                    } else {
                        theme.ink_muted
                    })
                    .child(text),
            )
        })
        .child(labeled_field("名前（任意）", name, theme))
        .child(render_mcp_adv_toggle(mode, adv_open, theme, cx))
        .when(adv_open, |this| this.child(adv_fields))
        .child(render_mcp_submit(
            connection, mcp_secret, enabled, &pane.hub, theme, cx,
        ))
        .into_any_element()
}

fn render_mcp_mode_fields(
    pane: &mut ConnectionsPane,
    mode: Mode,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (AnyElement, Option<&'static str>, McpTargetResult) {
    match mode {
        Mode::Npx => {
            let draft = pane.drafts.description_draft(
                cx,
                "add:mcp:npx",
                "例: npx -y @scope/mcp-server",
                None,
                theme,
            );
            let target = mcp_npx_target(draft.read(cx).text().trim());
            let el = labeled_field(
                "パッケージ名・npx コマンド・JSON",
                div()
                    .id("mcp-npx-editor")
                    .max_h(px(194.))
                    .overflow_y_scroll()
                    .child(draft),
                theme,
            );
            (
                el,
                Some(
                    "ホストに Node.js と npm が必要です。初回はパッケージの取得に時間がかかる場合があります。",
                ),
                target,
            )
        }
        Mode::Http => {
            let draft = pane.drafts.draft(
                cx,
                "add:mcp:url",
                "例: https://example.com/mcp",
                None,
                theme,
            );
            let target = mcp_http_target(draft.read(cx).text().trim());
            (labeled_field("サーバー URL", draft, theme), None, target)
        }
        Mode::Local => {
            let cmd = pane
                .drafts
                .draft(cx, "add:mcp:command", "例: node", None, theme);
            let args = pane.drafts.description_draft(
                cx,
                "add:mcp:args",
                r#"例: ["server.js", "--stdio"]"#,
                None,
                theme,
            );
            let target = mcp_local_target(cmd.read(cx).text().trim(), args.read(cx).text().trim());
            let el = div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(labeled_field("実行ファイル", cmd, theme))
                .child(labeled_field(
                    "引数（任意）",
                    div()
                        .id("mcp-local-args")
                        .max_h(px(124.))
                        .overflow_y_scroll()
                        .child(args),
                    theme,
                ))
                .into_any_element();
            (
                el,
                Some("空白を含む引数は引用符で囲むか、JSON 配列を貼り付けます。"),
                target,
            )
        }
    }
}

fn render_mcp_advanced_section(
    pane: &mut ConnectionsPane,
    mode: Mode,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (String, AnyElement, Option<&'static str>, McpExtraConfig) {
    if mode == Mode::Http {
        return render_mcp_http_advanced(pane, theme, cx);
    }
    render_mcp_process_advanced(pane, theme, cx)
}

fn render_mcp_http_advanced(
    pane: &mut ConnectionsPane,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (String, AnyElement, Option<&'static str>, McpExtraConfig) {
    let auth_hdr_d = pane.drafts.draft(
        cx,
        "add:mcp:auth-header",
        "空欄時は Authorization: Bearer",
        None,
        theme,
    );
    let secret_d = pane.drafts.masked_draft(
        cx,
        "add:mcp:secret",
        "認証トークン（任意・暗号保存）",
        true,
        theme,
    );
    let headers_d = pane.drafts.description_draft(
        cx,
        "add:mcp:headers",
        "例: X-Custom-Header: value（1行1件）",
        None,
        theme,
    );
    let auth_hdr = auth_hdr_d.read(cx).text().trim().to_owned();
    let secret = secret_d.read(cx).text().trim().to_owned();
    let headers_parsed = parse_mcp_kv(headers_d.read(cx).text(), ':');
    let adv_err = headers_parsed.as_ref().err().copied();
    let panel = div()
        .w_full()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(labeled_field("認証トークン（任意）", secret_d, theme))
        .child(labeled_field(
            "認証ヘッダー名（任意・空欄時は Bearer）",
            auth_hdr_d,
            theme,
        ))
        .child(labeled_field(
            "追加 HTTP ヘッダー（Key: Value）",
            headers_d,
            theme,
        ))
        .into_any_element();
    (
        secret,
        panel,
        adv_err,
        McpExtraConfig {
            auth_header: auth_hdr,
            headers: headers_parsed.unwrap_or_default(),
            ..Default::default()
        },
    )
}

fn render_mcp_process_advanced(
    pane: &mut ConnectionsPane,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (String, AnyElement, Option<&'static str>, McpExtraConfig) {
    let cwd_d = pane
        .drafts
        .draft(cx, "add:mcp:cwd", "例: C:/work/project", None, theme);
    let secret_env_d = pane.drafts.draft(
        cx,
        "add:mcp:secret-env",
        "例: GITHUB_TOKEN / API_KEY",
        None,
        theme,
    );
    let secret_d = pane.drafts.masked_draft(
        cx,
        "add:mcp:secret",
        "環境変数へ渡すシークレット値（任意）",
        true,
        theme,
    );
    let env_d = pane.drafts.description_draft(
        cx,
        "add:mcp:env",
        "例: NODE_ENV=production（1行1件）",
        None,
        theme,
    );
    let cwd = cwd_d.read(cx).text().trim().to_owned();
    let secret_env = secret_env_d.read(cx).text().trim().to_owned();
    let secret = secret_d.read(cx).text().trim().to_owned();
    let env_parsed = parse_mcp_kv(env_d.read(cx).text(), '=');
    let adv_err = env_parsed.as_ref().err().copied();
    let panel = div()
        .w_full()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(labeled_field("作業ディレクトリ (cwd)", cwd_d, theme))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(labeled_field(
                    "認証環境変数名",
                    secret_env_d,
                    theme,
                )))
                .child(div().flex_1().min_w_0().child(labeled_field(
                    "認証シークレット",
                    secret_d,
                    theme,
                ))),
        )
        .child(labeled_field("追加環境変数（KEY=VALUE）", env_d, theme))
        .into_any_element();
    (
        secret,
        panel,
        adv_err,
        McpExtraConfig {
            cwd,
            secret_env_var: secret_env,
            env: env_parsed.unwrap_or_default(),
            ..Default::default()
        },
    )
}

fn render_mcp_add_header(theme: &Theme, cx: &Context<ConnectionsPane>) -> gpui::Div {
    let pane = cx.entity();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("MCP サーバーを追加"),
        )
        .child(pill_button(
            "mcp-add-back",
            "戻る",
            Tone::Ghost,
            theme,
            move |_, cx| {
                pane.update(cx, |this, cx| {
                    this.form.add = AddFlow::Custom(ConnectionKind::Llm);
                    cx.notify();
                });
            },
        ))
}

fn render_mcp_mode_chips(mode: Mode, theme: &Theme, cx: &Context<ConnectionsPane>) -> gpui::Div {
    let choices = [
        (Mode::Npx, "mcp-mode-npx", "npx"),
        (Mode::Http, "mcp-mode-http", "URL"),
        (Mode::Local, "mcp-mode-local", "ローカルコマンド"),
    ]
    .into_iter()
    .map(|(choice, id, label)| {
        let pane = cx.entity();
        pill_button(
            id,
            label,
            if mode == choice {
                Tone::Primary
            } else {
                Tone::Ghost
            },
            theme,
            move |_, cx| {
                pane.update(cx, |this, cx| {
                    if let AddFlow::Mcp(form) = &mut this.form.add {
                        form.mode = choice;
                    }
                    cx.notify();
                });
            },
        )
    });
    div().flex().flex_wrap().gap(px(6.)).children(choices)
}

fn render_mcp_adv_toggle(
    mode: Mode,
    adv_open: bool,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> AnyElement {
    let pane = cx.entity();
    disclosure_row(
        "mcp-add-adv-toggle",
        if mode == Mode::Http {
            "認証・ヘッダー設定（任意）"
        } else {
            "環境変数・作業ディレクトリ・認証（任意）"
        },
        adv_open,
        theme,
        move |_, cx| {
            pane.update(cx, |this, cx| {
                if let AddFlow::Mcp(form) = &mut this.form.add {
                    form.advanced = !form.advanced;
                }
                cx.notify();
            });
        },
    )
}

fn render_mcp_submit(
    connection: Option<ConnectionInput>,
    mcp_secret: String,
    enabled: bool,
    hub: &gpui::Entity<crate::ui::Hub>,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let hub = hub.clone();
    let submit = cx.entity();
    div().flex().justify_end().child(pill_button(
        "mcp-add-submit",
        "追加して接続を確認",
        Tone::Primary.icon(Icon::Plus).enabled(enabled),
        theme,
        move |_, cx| {
            let Some(mut input) = connection.clone() else {
                return;
            };
            input.id = uuid::Uuid::new_v4().to_string();
            let id = input.id.clone();
            let secret = mcp_secret.clone();
            hub.update(cx, |hub, cx| {
                hub.upsert_connection("MCP の追加", input, cx);
                if !secret.is_empty() {
                    hub.register_secret(&id, &secret, cx);
                }
            });
            submit.update(cx, |this, cx| {
                this.form.pending_mcp = Some(id.clone());
                this.form.open_editor(id);
                this.form.add = AddFlow::Closed;
                this.open_dropdown = None;
                cx.notify();
            });
        },
    ))
}
