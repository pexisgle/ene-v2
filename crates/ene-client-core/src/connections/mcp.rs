//! Pure MCP server connection target parsing and input builder helpers.

use crate::domain::{
    ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind, McpConnection,
};

/// Advanced MCP server fields captured separately from the primary target input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpExtraConfig {
    pub cwd: String,
    pub secret_env_var: String,
    pub auth_header: String,
    pub env: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
}

/// Parsed MCP target and suggested display name (or `Ok(None)` when incomplete).
pub type McpTargetResult = Result<Option<(McpConnection, String)>, &'static str>;

/// Assembles a domain `ConnectionInput` for a new MCP server connection.
pub fn build_mcp_connection_input(
    target: McpTargetResult,
    extra: McpExtraConfig,
    chosen_name: &str,
) -> Option<ConnectionInput> {
    target.ok().flatten().map(|(mut config, suggested_name)| {
        config.cwd = extra.cwd;
        config.secret_env_var = extra.secret_env_var;
        config.auth_header = extra.auth_header;
        config.env = extra.env.into_iter().collect();
        config.headers = extra.headers.into_iter().collect();
        let name = if chosen_name.is_empty() {
            suggested_name
        } else {
            chosen_name.to_owned()
        };
        ConnectionInput {
            id: String::new(),
            name,
            enabled: true,
            expected_revision: 0,
            kind: ConnectionKind::Mcp,
            config: Some(ConnectionConfig::Mcp(config)),
        }
    })
}

/// Parses `KEY=VALUE` or `Header: Value` multi-line MCP advanced settings.
pub fn parse_mcp_kv(raw: &str, sep: char) -> Result<Vec<(String, String)>, &'static str> {
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

fn mcp_endpoint(url: String) -> ConnectionEndpoint {
    ConnectionEndpoint {
        adapter: "mcp".into(),
        endpoint: url,
        options_schema: String::new(),
        options: "{}".into(),
    }
}

/// Builds an stdio MCP target tuple `(config, suggested_name)`.
pub fn mcp_process_target(command: &str, args: Vec<String>, name: &str) -> (McpConnection, String) {
    (
        McpConnection {
            command: command.to_owned(),
            args,
            endpoint: Some(mcp_endpoint(String::new())),
            ..Default::default()
        },
        name.chars().take(128).collect(),
    )
}

/// Parses an HTTP/HTTPS MCP server URL target.
pub fn mcp_http_target(input: &str) -> McpTargetResult {
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
        McpConnection {
            endpoint: Some(mcp_endpoint(url.to_string())),
            ..Default::default()
        },
        name.chars().take(128).collect(),
    )))
}

/// Parses a local executable command + argument string for MCP stdio transport.
pub fn mcp_local_target(command: &str, input: &str) -> McpTargetResult {
    if command.is_empty() {
        return Ok(None);
    }
    let args = parse_process_args(input)?;
    let command = command.trim_matches('"');
    if command.is_empty() {
        return Err("実行ファイルを入力してください。");
    }
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    Ok(Some(mcp_process_target(command, args, name)))
}

/// Parses process arguments from either a JSON string array or shell-free quoted text.
pub fn parse_process_args(input: &str) -> Result<Vec<String>, &'static str> {
    let input = input.trim();
    if input.starts_with('[') {
        serde_json::from_str(input).map_err(|_| "引数を文字列の JSON 配列で入力してください。")
    } else {
        split_args(input)
    }
}

/// Parses an `npx` package, command line, or Claude-style `mcpServers` JSON snippet.
pub fn mcp_npx_target(input: &str) -> McpTargetResult {
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
    Ok(Some(mcp_process_target("npx", args, &name)))
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
        let (config, name) = mcp_npx_target("@scope/server").unwrap().unwrap();
        assert_eq!(config.command, "npx");
        assert_eq!(config.args, ["-y", "@scope/server"]);
        assert_eq!(name, "@scope/server");

        let (config, _) = mcp_npx_target(r#"npx.cmd -y package "C:\My Tools\folder" C:\bare "" "#)
            .unwrap()
            .unwrap();
        assert_eq!(
            config.args,
            ["-y", "package", r"C:\My Tools\folder", r"C:\bare", ""]
        );

        let (config, name) = mcp_npx_target(
            r#"{"mcpServers":{"Fixture":{"command":"npx","args":["pkg","--flag","a b"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.args, ["-y", "pkg", "--flag", "a b"]);
        assert_eq!(name, "Fixture");
        assert!(mcp_npx_target("npx -y").is_err());
        assert!(mcp_npx_target(r#"npx pkg "open"#).is_err());
        assert!(mcp_npx_target(r#"{"command":"npx","args":["pkg"],"env":{"A":"B"}}"#).is_err());
        assert!(mcp_npx_target(r#"{"command":"npx","args":["pkg"],"disabled":true}"#).is_err());
    }

    #[test]
    fn remote_and_local_are_independent_connection_modes() {
        let (remote, name) = mcp_http_target("https://example.com/mcp").unwrap().unwrap();
        assert_eq!(
            remote.endpoint.as_ref().unwrap().endpoint,
            "https://example.com/mcp"
        );
        assert_eq!(name, "example.com");
        assert_eq!(remote.command.len(), 0);
        assert!(mcp_http_target("file:///tmp/mcp").is_err());
        let (local, _) = mcp_local_target("node", r#"["C:/My Tools/mcp.js","--stdio"]"#)
            .unwrap()
            .unwrap();
        assert_eq!(local.command, "node");
        assert_eq!(local.args, ["C:/My Tools/mcp.js", "--stdio"]);
        assert!(mcp_local_target("node", "[").is_err());
    }

    #[test]
    fn npx_normalizes_windows_command_wrappers_without_using_a_shell() {
        let (config, name) = mcp_npx_target(
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

        let (config, _) = mcp_npx_target(r#"cmd.exe /D /S /C npx.cmd pkg "a b""#)
            .unwrap()
            .unwrap();
        assert_eq!(config.args, ["-y", "pkg", "a b"]);
        let (config, name) = mcp_npx_target(
            r#"{"servers":{"fixture":{"type":"stdio","command":"cmd.exe","args":["/c","npx","pkg"]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(name, "fixture");
        assert_eq!(config.args, ["-y", "pkg"]);
        assert!(mcp_npx_target("cmd /c echo hello").is_err());
        assert!(mcp_npx_target("cmd /k npx pkg").is_err());
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
