use super::SettingsStudio;
use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{Tone, pill_button};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Mode {
    Npx,
    Http,
    Local,
}

impl SettingsStudio {
    pub(super) fn render_mcp_add(
        &mut self,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = self.mcp_add_mode;
        let studio = cx.entity();
        let choice_studio = studio.clone();
        let choices = [
            (Mode::Npx, "mcp-mode-npx", "npx"),
            (Mode::Http, "mcp-mode-http", "URL"),
            (Mode::Local, "mcp-mode-local", "ローカルコマンド"),
        ]
        .into_iter()
        .map(move |(choice, id, label)| {
            let studio = choice_studio.clone();
            pill_button(
                id,
                label,
                None,
                if mode == choice {
                    Tone::Primary
                } else {
                    Tone::Ghost
                },
                true,
                theme,
                move |_, cx| {
                    studio.update(cx, |this, cx| {
                        this.mcp_add_mode = choice;
                        cx.notify();
                    });
                },
            )
        });
        let name = self.draft(cx, "add:mcp:name", "空欄なら接続先から設定", None, theme);
        let (fields, helper, target) = match mode {
            Mode::Npx => {
                let draft = self.description_draft(
                    cx,
                    "add:mcp:npx",
                    "例: npx -y @scope/mcp-server",
                    None,
                    theme,
                );
                let target = npx_target(draft.read(cx).text().trim());
                (
                    labeled_field(
                        "パッケージ名・npx コマンド・JSON",
                        div()
                            .id("mcp-npx-editor")
                            .max_h(px(194.))
                            .overflow_y_scroll()
                            .child(draft),
                        theme,
                    ),
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
                (
                    div()
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
                        .into_any_element(),
                    Some("空白を含む引数は引用符で囲むか、JSON 配列を貼り付けます。"),
                    target,
                )
            }
        };
        let mut error = target.as_ref().err().copied();
        let chosen_name = name.read(cx).text().trim();
        if chosen_name.chars().count() > 128 {
            error = Some("名前は128文字以内で入力してください。");
        }
        let connection = target.ok().flatten().map(|(config, suggested_name)| {
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
        });
        let enabled = live && error.is_none() && connection.is_some();
        let hub = self.hub.clone();
        let submit = studio.clone();
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
            .child(
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
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio.update(cx, |this, cx| {
                                this.custom_kind = w::ConnectionKind::Llm;
                                cx.notify();
                            });
                        },
                    )),
            )
            .child(div().flex().flex_wrap().gap(px(6.)).children(choices))
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
            .child(div().flex().justify_end().child(pill_button(
                "mcp-add-submit",
                "追加して接続を確認",
                Some(Icon::Plus),
                Tone::Primary,
                enabled,
                theme,
                move |_, cx| {
                    let Some(mut input) = connection.clone() else {
                        return;
                    };
                    input.id = uuid::Uuid::new_v4().to_string();
                    let id = input.id.clone();
                    hub.update(cx, |hub, cx| hub.upsert_connection("MCP の追加", input, cx));
                    submit.update(cx, |this, cx| {
                        this.pending_mcp = Some(id.clone());
                        this.expanded_connection_id = Some(id);
                        this.adding_connection = false;
                        this.open_dropdown = None;
                        cx.notify();
                    });
                },
            )))
            .into_any_element()
    }
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
    let args = if input.starts_with('[') {
        serde_json::from_str::<Vec<String>>(input)
            .map_err(|_| "引数を文字列の JSON 配列で入力してください。")?
    } else {
        split_args(input)?
    };
    let command = command.trim_matches('"');
    if command.is_empty() {
        return Err("実行ファイルを入力してください。");
    }
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    Ok(Some(process_target(command, args, name)))
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
            let (name, config) = servers.iter().next().expect("one MCP server");
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
}
