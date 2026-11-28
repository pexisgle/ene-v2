//! Pure MCP server connection target parsing and input builder helpers.

use crate::domain::{ConnectionConfig, ConnectionEndpoint, ConnectionInput, McpConnection};
use ene_domain::settings::is_npx_command;
use std::collections::BTreeMap;

/// Advanced MCP server fields captured separately from the primary target input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpExtraConfig {
    /// Working directory for the server command; empty inherits the host's.
    pub cwd: String,
    /// Environment variable receiving the registered secret in the server process.
    pub secret_env_var: String,
    /// Header name the stored secret is sent in.
    pub auth_header: String,
    /// Environment variables added to the command.
    pub env: BTreeMap<String, String>,
    /// HTTP headers sent to a remote server.
    pub headers: BTreeMap<String, String>,
}

impl McpExtraConfig {
    /// Apply validated advanced settings to a connection being created or edited.
    pub fn apply_to(self, input: &mut ConnectionInput) {
        if let Some(ConnectionConfig::Mcp(config)) = input.config.as_mut() {
            config.cwd = self.cwd;
            config.secret_env_var = self.secret_env_var;
            config.auth_header = self.auth_header;
            config.env = self.env;
            config.headers = self.headers;
        }
    }
}

/// Parsed MCP target and suggested display name (or `Ok(None)` when incomplete).
pub type McpTargetResult = Result<Option<(McpConnection, String)>, &'static str>;

/// Assembles a domain `ConnectionInput` for a new MCP server connection.
pub fn build_mcp_connection_input(
    target: McpTargetResult,
    extra: McpExtraConfig,
    chosen_name: &str,
) -> Option<ConnectionInput> {
    target.ok().flatten().map(|(config, suggested_name)| {
        let name = if chosen_name.is_empty() {
            suggested_name
        } else {
            chosen_name.to_owned()
        };
        let mut input = ConnectionInput {
            id: String::new(),
            name,
            enabled: true,
            expected_revision: 0,
            config: Some(ConnectionConfig::Mcp(config)),
        };
        extra.apply_to(&mut input);
        input
    })
}

/// The reason a line in an MCP header or environment map could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpKvError {
    /// The input line does not contain the required delimiter.
    #[error("the line is missing its key-value separator")]
    MissingSeparator,
    /// The input has no key before its delimiter.
    #[error("the key is empty")]
    EmptyKey,
}

impl McpKvError {
    /// The shared message shown for an invalid advanced settings line.
    pub const fn message(self, separator: char) -> &'static str {
        match self {
            Self::EmptyKey => "キー名が空の行があります。",
            Self::MissingSeparator if separator == ':' => {
                "ヘッダーは 1 行ごとに「Header: Value」形式で入力してください。"
            }
            Self::MissingSeparator => "環境変数は 1 行ごとに「KEY=VALUE」形式で入力してください。",
        }
    }
}

/// Parses one header or environment variable per line. A later key replaces an earlier value.
pub fn parse_mcp_kv(raw: &str, separator: char) -> Result<BTreeMap<String, String>, McpKvError> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (key, value) = line
                .split_once(separator)
                .ok_or(McpKvError::MissingSeparator)?;
            let key = key.trim();
            if key.is_empty() {
                return Err(McpKvError::EmptyKey);
            }
            Ok((key.to_owned(), value.trim().to_owned()))
        })
        .collect()
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
            endpoint: mcp_endpoint(String::new()),
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
            endpoint: mcp_endpoint(url.to_string()),
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
    let (name, command, mut args) = if input.starts_with('{') {
        npx_from_json(input)?
    } else {
        let mut args = split_args(input)?;
        let (command, args) = if args
            .first()
            .is_some_and(|arg| is_npx_command(arg) || is_cmd(arg))
        {
            let command = args.remove(0);
            npx_launch(command, args)?
        } else {
            ("npx".into(), args)
        };
        (None, command, args)
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
    Ok(Some(mcp_process_target(&command, args, &name)))
}

fn is_cmd(command: &str) -> bool {
    command.eq_ignore_ascii_case("cmd") || command.eq_ignore_ascii_case("cmd.exe")
}

fn npx_launch(
    mut command: String,
    mut args: Vec<String>,
) -> Result<(String, Vec<String>), &'static str> {
    let invalid = "npx、または Windows 向けの cmd /c npx を指定してください。";
    if is_cmd(&command) {
        let mut rest = args
            .into_iter()
            .skip_while(|arg| arg.eq_ignore_ascii_case("/d") || arg.eq_ignore_ascii_case("/s"));
        if !rest
            .next()
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/c"))
        {
            return Err(invalid);
        }
        command = rest.next().ok_or(invalid)?;
        args = rest.collect();
    }
    if !is_npx_command(&command) {
        return Err(invalid);
    }
    if !command.contains(['/', '\\']) {
        command = "npx".into();
    }
    Ok((command, args))
}

fn npx_from_json(input: &str) -> Result<(Option<String>, String, Vec<String>), &'static str> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: Option<String>,
        #[serde(rename = "type")]
        transport: Option<String>,
    }
    let mut value: serde_json::Value =
        serde_json::from_str(input).map_err(|_| "MCP の JSON 設定を確認してください。")?;
    let nested = value.as_object_mut().and_then(|value| {
        value
            .remove("mcpServers")
            .or_else(|| value.remove("servers"))
    });
    let (name, config) = match nested {
        Some(serde_json::Value::Object(servers)) if servers.len() == 1 => {
            let (name, config) = servers
                .into_iter()
                .next()
                .ok_or("JSON にはサーバーを1つだけ指定してください。")?;
            (Some(name), config)
        }
        Some(_) => return Err("JSON にはサーバーを1つだけ指定してください。"),
        None => (None, value),
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
    let (command, args) = npx_launch(config.command, config.args)?;
    Ok((name, command, args))
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
        assert_eq!(remote.endpoint.endpoint, "https://example.com/mcp");
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
    fn npx_import_preserves_explicit_installations_and_literal_arguments() {
        for input in [
            r#""C:\My Tools\npx.cmd" -y pkg "a b""#,
            r#"cmd /D /c "C:\My Tools\npx.cmd" pkg "a b""#,
            r#"{"mcpServers":{"fixture":{"command":"C:\\My Tools\\npx.cmd","args":["pkg","a b"]}}}"#,
        ] {
            let (config, _) = mcp_npx_target(input).unwrap().unwrap();
            assert_eq!(config.command, r"C:\My Tools\npx.cmd");
            assert_eq!(config.args, ["-y", "pkg", "a b"]);
        }
    }

    #[test]
    fn parse_mcp_kv_accepts_headers_and_env_and_rejects_malformed_lines() {
        let env = parse_mcp_kv("FOO=bar\n  # comment=ok\nEMPTY=\n\n", '=').unwrap();
        assert_eq!(env.len(), 3);
        assert_eq!(env["FOO"], "bar");
        assert_eq!(env["# comment"], "ok");
        assert_eq!(env["EMPTY"], "");
        let headers = parse_mcp_kv(
            "X-Tenant: discarded\r\nX-Mode: strict\nX-Tenant: acme:two",
            ':',
        )
        .unwrap();
        assert_eq!(
            headers,
            BTreeMap::from([
                ("X-Tenant".into(), "acme:two".into()),
                ("X-Mode".into(), "strict".into()),
            ])
        );
        assert!(parse_mcp_kv("INVALID_LINE_WITHOUT_SEP", '=').is_err());
        assert!(parse_mcp_kv(": missing-key", ':').is_err());
    }
}
