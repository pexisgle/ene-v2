use super::SettingsStudio;
use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{Tone, disclosure_row, pill_button};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Mode {
    Npx,
    Http,
    Local,
}

type ExtraMcpCfg = (
    String,
    String,
    String,
    Vec<(String, String)>,
    Vec<(String, String)>,
);

impl SettingsStudio {
    pub(super) fn render_mcp_add(
        &mut self,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = self.connection_form.mcp_add_mode;
        let name = self.draft(cx, "add:mcp:name", "空欄なら接続先から設定", None, theme);
        let (fields, helper, target) = self.render_mcp_mode_fields(mode, theme, cx);
        let adv_open = self.connection_form.mcp_add_advanced_open;
        let (mcp_secret, adv_fields, adv_error, extra_cfg) =
            self.render_mcp_advanced_section(mode, theme, cx);

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
            .rounded(px(14.))
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
                connection, mcp_secret, enabled, &self.hub, theme, cx,
            ))
            .into_any_element()
    }

    fn render_mcp_mode_fields(
        &mut self,
        mode: Mode,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<&'static str>, Target) {
        match mode {
            Mode::Npx => {
                let draft = self.description_draft(
                    cx,
                    "add:mcp:npx",
                    "例: npx -y @scope/mcp-server",
                    None,
                    theme,
                );
                let target = npx_target(draft.read(cx).text().trim());
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
                let draft = self.draft(
                    cx,
                    "add:mcp:url",
                    "例: https://example.com/mcp",
                    None,
                    theme,
                );
                let target = http_target(draft.read(cx).text().trim());
                (labeled_field("サーバー URL", draft, theme), None, target)
            }
            Mode::Local => {
                let cmd = self.draft(cx, "add:mcp:command", "例: node", None, theme);
                let args = self.description_draft(
                    cx,
                    "add:mcp:args",
                    r#"例: ["server.js", "--stdio"]"#,
                    None,
                    theme,
                );
                let target = local_target(cmd.read(cx).text().trim(), args.read(cx).text().trim());
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
        &mut self,
        mode: Mode,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (String, AnyElement, Option<&'static str>, ExtraMcpCfg) {
        if mode == Mode::Http {
            return self.render_mcp_http_advanced(theme, cx);
        }
        self.render_mcp_process_advanced(theme, cx)
    }

    fn render_mcp_http_advanced(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (String, AnyElement, Option<&'static str>, ExtraMcpCfg) {
        let auth_hdr_d = self.draft(
            cx,
            "add:mcp:auth-header",
            "空欄時は Authorization: Bearer",
            None,
            theme,
        );
        let secret_d = self.masked_draft(
            cx,
            "add:mcp:secret",
            "認証トークン（任意・暗号保存）",
            true,
            theme,
        );
        let headers_d = self.description_draft(
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
            .rounded(px(10.))
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
            (
                String::new(),
                String::new(),
                auth_hdr,
                Vec::new(),
                headers_parsed.unwrap_or_default(),
            ),
        )
    }

    fn render_mcp_process_advanced(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (String, AnyElement, Option<&'static str>, ExtraMcpCfg) {
        let cwd_d = self.draft(cx, "add:mcp:cwd", "例: C:/work/project", None, theme);
        let secret_env_d = self.draft(
            cx,
            "add:mcp:secret-env",
            "例: GITHUB_TOKEN / API_KEY",
            None,
            theme,
        );
        let secret_d = self.masked_draft(
            cx,
            "add:mcp:secret",
            "環境変数へ渡すシークレット値（任意）",
            true,
            theme,
        );
        let env_d = self.description_draft(
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
            .rounded(px(10.))
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
            (
                cwd,
                secret_env,
                String::new(),
                env_parsed.unwrap_or_default(),
                Vec::new(),
            ),
        )
    }
}

fn render_mcp_add_header(theme: &Theme, cx: &Context<SettingsStudio>) -> gpui::Div {
    let studio = cx.entity();
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
                studio.update(cx, |this, cx| {
                    this.connection_form.custom_kind = w::ConnectionKind::Llm;
                    cx.notify();
                });
            },
        ))
}

fn render_mcp_mode_chips(mode: Mode, theme: &Theme, cx: &Context<SettingsStudio>) -> gpui::Div {
    let choices = [
        (Mode::Npx, "mcp-mode-npx", "npx"),
        (Mode::Http, "mcp-mode-http", "URL"),
        (Mode::Local, "mcp-mode-local", "ローカルコマンド"),
    ]
    .into_iter()
    .map(|(choice, id, label)| {
        let studio = cx.entity();
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
                studio.update(cx, |this, cx| {
                    this.connection_form.mcp_add_mode = choice;
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
    cx: &Context<SettingsStudio>,
) -> AnyElement {
    let studio = cx.entity();
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
            studio.update(cx, |this, cx| {
                this.connection_form.mcp_add_advanced_open =
                    !this.connection_form.mcp_add_advanced_open;
                cx.notify();
            });
        },
    )
}

fn render_mcp_submit(
    connection: Option<w::ConnectionInput>,
    mcp_secret: String,
    enabled: bool,
    hub: &gpui::Entity<crate::ui::Hub>,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
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
                    hub.register_secret(id.clone(), secret, cx);
                }
            });
            submit.update(cx, |this, cx| {
                this.connection_form.pending_mcp = Some(id.clone());
                this.connection_form.expanded_connection_id = Some(id);
                this.connection_form.adding_connection = false;
                this.connection_form.mcp_add_advanced_open = false;
                this.open_dropdown = None;
                cx.notify();
            });
        },
    ))
}

fn build_mcp_connection_input(
    target: Target,
    extra_cfg: ExtraMcpCfg,
    chosen_name: &str,
) -> Option<w::ConnectionInput> {
    target.ok().flatten().map(|(mut config, suggested_name)| {
        let (cwd, secret_env_var, auth_header, env, headers) = extra_cfg;
        config.cwd = cwd;
        config.secret_env_var = secret_env_var;
        config.auth_header = auth_header;
        config.env = env.into_iter().collect();
        config.headers = headers.into_iter().collect();
        let name = if chosen_name.is_empty() {
            suggested_name
        } else {
            chosen_name.to_owned()
        };
        w::ConnectionInput {
            name,
            enabled: true,
            kind: w::ConnectionKind::Mcp.into(),
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Mcp(Box::new(config))),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
    })
}

fn parse_mcp_kv(raw: &str, sep: char) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once(sep) else {
            return Err(if sep == ':' {
                "追加ヘッダーは 1 行ごとに「Header: Value」形式で入力してください。"
            } else {
                "追加環境変数は 1 行ごとに「KEY=VALUE」形式で入力してください。"
            });
        };
        let key = k.trim();
        if key.is_empty() {
            return Err("キー名が空の行があります。");
        }
        out.push((key.to_owned(), v.trim().to_owned()));
    }
    Ok(out)
}

type Target = Result<Option<(w::McpConnection, String)>, &'static str>;

fn endpoint(url: String) -> w::ConnectionEndpoint {
    w::ConnectionEndpoint {
        adapter: "mcp".into(),
        endpoint: url,
        options: "{}".into(),
        ..Default::default()
    }
}

fn process_target(command: &str, args: Vec<String>, name: &str) -> (w::McpConnection, String) {
    (
        w::McpConnection {
            command: command.to_owned(),
            args,
            endpoint: Some(endpoint(String::new())).into(),
            ..Default::default()
        },
        name.chars().take(128).collect(),
    )
}

fn http_target(input: &str) -> Target {
    if input.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(input)
        .map_err(|_| "http:// または https:// で始まる URL を入力してください。")?;
    let name = url
        .host_str()
        .filter(|_| matches!(url.scheme(), "http" | "https"))
        .ok_or("http:// または https:// で始まる URL を入力してください。")?;
    Ok(Some((
        w::McpConnection {
            endpoint: Some(endpoint(url.to_string())).into(),
            ..Default::default()
        },
        name.chars().take(128).collect(),
    )))
}

fn local_target(command: &str, input: &str) -> Target {
    if command.is_empty() {
        return Ok(None);
    }
    let args = parse_process_args(input)?;
    let command = command.trim_matches('"');
    if command.is_empty() {
        return Err("実行ファイルを入力してください。");
    }
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    Ok(Some(process_target(command, args, name)))
}

pub(super) fn parse_process_args(input: &str) -> Result<Vec<String>, &'static str> {
    let input = input.trim();
    if input.starts_with('[') {
        serde_json::from_str(input).map_err(|_| "引数を文字列の JSON 配列で入力してください。")
    } else {
        split_args(input)
    }
}

fn npx_target(input: &str) -> Target {
    if input.is_empty() {
        return Ok(None);
    }
    let (name, mut args) = if input.starts_with('{') {
        npx_from_json(input)?
    } else {
        let args = split_args(input)?;
        let args = match args.as_slice() {
            [command, tail @ ..] if is_npx(command) || is_cmd(command) => {
                npx_args(command, tail)?.to_vec()
            }
            _ => args,
        };
        (None, args)
    };
    let package = args
        .iter()
        .find(|arg| !arg.is_empty() && !arg.starts_with('-'))
        .ok_or("起動する npm パッケージを指定してください。")?;
    if package.starts_with("http://") || package.starts_with("https://") {
        return Err("サーバー URL を登録する場合は「URL」を選んでください。");
    }
    let name = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            if package.contains(char::is_whitespace) {
                "npx MCP".into()
            } else {
                package.clone()
            }
        });
    if !args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "-y" | "--yes" | "--yes=true"))
    {
        args.insert(0, "-y".into());
    }
    Ok(Some(process_target("npx", args, &name)))
}

fn is_npx(command: &str) -> bool {
    ["npx", "npx.cmd", "npx.exe", "npx.ps1"]
        .iter()
        .any(|program| command.eq_ignore_ascii_case(program))
}

fn is_cmd(command: &str) -> bool {
    command.eq_ignore_ascii_case("cmd") || command.eq_ignore_ascii_case("cmd.exe")
}

fn npx_args<'a>(command: &str, args: &'a [String]) -> Result<&'a [String], &'static str> {
    if is_npx(command) {
        return Ok(args);
    }
    // Accept the documented Windows wrapper, then launch npx directly on the host.
    if is_cmd(command) {
        let flags = args
            .iter()
            .take_while(|arg| arg.eq_ignore_ascii_case("/d") || arg.eq_ignore_ascii_case("/s"))
            .count();
        if let [run, launcher, tail @ ..] = &args[flags..]
            && run.eq_ignore_ascii_case("/c")
            && is_npx(launcher)
        {
            return Ok(tail);
        }
    }
    Err("npx、または Windows 向けの cmd /c npx を指定してください。")
}

fn npx_from_json(input: &str) -> Result<(Option<String>, Vec<String>), &'static str> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        cwd: Option<String>,
        #[serde(rename = "type")]
        transport: Option<String>,
    }
    let value: serde_json::Value =
        serde_json::from_str(input).map_err(|_| "MCP の JSON 設定を確認してください。")?;
    let (name, config) =
        if let Some(servers) = value.get("mcpServers").or_else(|| value.get("servers")) {
            let servers = servers
                .as_object()
                .filter(|servers| servers.len() == 1)
                .ok_or("JSON にはサーバーを1つだけ指定してください。")?;
            let Some((name, config)) = servers.iter().next() else {
                return Err("JSON にはサーバーを1つだけ指定してください。");
            };
            (Some(name.clone()), config.clone())
        } else {
            (None, value)
        };
    let config: Config = serde_json::from_value(config)
        .map_err(|_| "JSON の command と args を確認してください。")?;
    if config
        .transport
        .as_deref()
        .is_some_and(|transport| transport != "stdio")
    {
        return Err("この欄では stdio 接続の JSON を使えます。");
    }
    if !config.env.is_empty() || config.cwd.is_some() {
        return Err(
            "env や cwd を指定するサーバーは、ホストの実行環境を設定してからコマンドを登録してください。",
        );
    }
    Ok((name, npx_args(&config.command, &config.args)?.to_vec()))
}

/// Splits arguments without a shell. Backslashes stay literal for Windows paths.
fn split_args(input: &str) -> Result<Vec<String>, &'static str> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in input.chars() {
        match quote {
            Some(end) if ch == end => quote = None,
            Some(_) => arg.push(ch),
            None if matches!(ch, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut arg));
                    started = false;
                }
            }
            None => {
                arg.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("引数を囲む引用符を閉じてください。");
    }
    if started {
        args.push(arg);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npx_accepts_packages_commands_and_json_without_losing_windows_arguments() {
        let (config, name) = npx_target("@scope/server").unwrap().unwrap();
        assert_eq!(config.command, "npx");
        assert_eq!(config.args, ["-y", "@scope/server"]);
        assert_eq!(name, "@scope/server");

        let (config, _) = npx_target(r#"npx.cmd -y package "C:\My Tools\folder" C:\bare "" "#)
            .unwrap()
            .unwrap();
        assert_eq!(
            config.args,
            ["-y", "package", r"C:\My Tools\folder", r"C:\bare", ""]
        );

        let (config, name) = npx_target(
            r#"{"mcpServers":{"Fixture":{"command":"npx","args":["pkg","--flag","a b"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.args, ["-y", "pkg", "--flag", "a b"]);
        assert_eq!(name, "Fixture");
        assert!(npx_target("npx -y").is_err());
        assert!(npx_target(r#"npx pkg "open"#).is_err());
        assert!(npx_target(r#"{"command":"npx","args":["pkg"],"env":{"A":"B"}}"#).is_err());
        assert!(npx_target(r#"{"command":"npx","args":["pkg"],"disabled":true}"#).is_err());
    }

    #[test]
    fn remote_and_local_are_independent_connection_modes() {
        let (remote, name) = http_target("https://example.com/mcp").unwrap().unwrap();
        assert_eq!(remote.endpoint.endpoint, "https://example.com/mcp");
        assert_eq!(name, "example.com");
        assert!(remote.command.is_empty());
        assert!(http_target("file:///tmp/mcp").is_err());
        let (local, _) = local_target("node", r#"["C:/My Tools/mcp.js","--stdio"]"#)
            .unwrap()
            .unwrap();
        assert_eq!(local.command, "node");
        assert_eq!(local.args, ["C:/My Tools/mcp.js", "--stdio"]);
        assert!(local_target("node", "[").is_err());
    }

    #[test]
    fn npx_normalizes_windows_command_wrappers_without_using_a_shell() {
        let (config, name) = npx_target(
            r#"{"mcpServers":{"filesystem":{"command":"cmd","args":["/c","npx","-y","@modelcontextprotocol/server-filesystem","C:\\My Tools\\日本語 & space"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(name, "filesystem");
        assert_eq!(config.command, "npx");
        assert_eq!(
            config.args,
            [
                "-y",
                "@modelcontextprotocol/server-filesystem",
                r"C:\My Tools\日本語 & space"
            ]
        );

        let (config, _) = npx_target(r#"cmd.exe /D /S /C npx.cmd pkg "a b""#)
            .unwrap()
            .unwrap();
        assert_eq!(config.args, ["-y", "pkg", "a b"]);
        let (config, name) = npx_target(
            r#"{"servers":{"fixture":{"type":"stdio","command":"cmd.exe","args":["/c","npx","pkg"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(name, "fixture");
        assert_eq!(config.args, ["-y", "pkg"]);
        assert!(npx_target("cmd /c echo hello").is_err());
        assert!(npx_target("cmd /k npx pkg").is_err());
    }

    #[test]
    fn parse_mcp_kv_accepts_headers_and_env_and_rejects_malformed_lines() {
        let env = parse_mcp_kv("FOO=bar\n  # comment=ok\nEMPTY=\n\n", '=').unwrap();
        assert_eq!(env.len(), 3);
        assert_eq!(env[0], ("FOO".into(), "bar".into()));
        let headers = parse_mcp_kv("X-Tenant: acme\nX-Mode: strict", ':').unwrap();
        assert_eq!(
            headers,
            vec![
                ("X-Tenant".into(), "acme".into()),
                ("X-Mode".into(), "strict".into()),
            ]
        );
        assert!(parse_mcp_kv("INVALID_LINE_WITHOUT_SEP", '=').is_err());
        assert!(parse_mcp_kv(": missing-key", ':').is_err());
    }
}
