//! ECS-free MCP transport connection opening, backoff calculation, and tool naming helpers.

use backon::{BackoffBuilder as _, ExponentialBuilder};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use rig_agent::tool::server::ToolServerHandle;
use rig_rmcp::{McpClientHandler, McpConnection};
use rmcp::{
    model::ClientConfig,
    transport::{
        child_process::TokioChildProcess,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use std::{collections::BTreeMap, time::Duration};

#[cfg(windows)]
use super::npx::CREATE_NO_WINDOW;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub revision: u64,
    pub credential_generation: Option<u64>,
    pub owner: String,
}

/// What one settings row asks the host to connect to.
#[derive(Clone, Debug)]
pub struct Desired {
    pub revision: u64,
    pub credential_generation: Option<u64>,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub auth_header: String,
    pub secret_env_var: String,
    pub endpoint: String,
    pub secret: Option<String>,
    pub allowed: Vec<String>,
}

/// Live inspection state returned to RPC connection probes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckState {
    Connected(Vec<String>),
    Failed(String),
    Connecting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectFailure {
    pub reason: String,
    pub config_error: bool,
}

/// Jitters retries within the second half of the exponential delay window.
pub fn backoff_delay(attempts: u32) -> Duration {
    // Backon adds up to the base delay as jitter. Halving the bounds keeps
    // the first retry within 0.5–1s and the capped retry within 30–60s.
    ExponentialBuilder::new()
        .with_min_delay(INITIAL_BACKOFF / 2)
        .with_max_delay(MAX_BACKOFF / 2)
        .with_jitter()
        .without_max_times()
        .build()
        // The seventh attempt reaches the cap.
        .nth(attempts.saturating_sub(1).min(6) as usize)
        .unwrap_or(MAX_BACKOFF)
}

pub fn is_config_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("auth required")
        || lower.contains("authrequired")
        || lower.contains("insufficient scope")
        || lower.contains("insufficientscope")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("cannot find the file")
        || lower.contains("no such file")
        || lower.contains("not found on path")
        || lower.contains("neither a command nor an endpoint")
}

/// Opens either a stdio child-process MCP server or a streamable HTTP MCP server.
pub async fn open(
    want: &Desired,
    registry: ToolServerHandle,
    http_client: reqwest::Client,
) -> Result<McpConnection, ConnectFailure> {
    if !want.command.is_empty() {
        Box::pin(open_stdio(want, registry)).await
    } else if !want.endpoint.is_empty() {
        Box::pin(open_http(want, registry, http_client)).await
    } else {
        Err(ConnectFailure {
            reason: "the MCP connection names neither a command nor an endpoint".to_owned(),
            config_error: true,
        })
    }
}

async fn open_stdio(
    want: &Desired,
    registry: ToolServerHandle,
) -> Result<McpConnection, ConnectFailure> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    #[cfg(windows)]
    let mut command = if super::npx::is_npx_command(&want.command) {
        super::npx::build_command(want).await?
    } else {
        let mut command = tokio::process::Command::new(&want.command);
        command.args(&want.args);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = tokio::process::Command::new(&want.command);
        command.args(&want.args);
        command
    };
    if !want.cwd.is_empty() {
        command.current_dir(&want.cwd);
    }
    if !want.env.is_empty() {
        command.envs(&want.env);
    }
    if let Some(secret) = want.secret.as_deref().filter(|s| !s.is_empty())
        && !want.secret_env_var.is_empty()
    {
        command.env(&want.secret_env_var, secret);
    }
    let mut wrap = process_wrap_rmcp::tokio::CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(process_wrap_rmcp::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        // JobObject::pre_spawn sets the process creation flags from whatever
        // `CreationFlags` wrapper is registered, and zero when none is. The raw
        // `Command::creation_flags` a caller set would be overwritten, so the
        // suppression has to be registered here to survive.
        wrap.wrap(process_wrap_rmcp::tokio::CreationFlags(
            windows::Win32::System::Threading::PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW),
        ));
        wrap.wrap(process_wrap_rmcp::tokio::JobObject);
    }
    wrap.wrap(process_wrap_rmcp::tokio::KillOnDrop);
    let transport = TokioChildProcess::new(wrap).map_err(|error| ConnectFailure {
        config_error: error.kind() == std::io::ErrorKind::NotFound,
        reason: error.to_string(),
    })?;
    Box::pin(handler.connect(transport)).await.map_err(|error| {
        let reason = error.to_string();
        ConnectFailure {
            config_error: is_config_error_message(&reason),
            reason,
        }
    })
}

async fn open_http(
    want: &Desired,
    registry: ToolServerHandle,
    client: reqwest::Client,
) -> Result<McpConnection, ConnectFailure> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    let mut config = StreamableHttpClientTransportConfig::with_uri(want.endpoint.clone());
    for (name, value) in &want.headers {
        let header_name =
            HeaderName::from_bytes(name.trim().as_bytes()).map_err(|error| ConnectFailure {
                reason: format!("invalid MCP header `{name}`: {error}"),
                config_error: true,
            })?;
        let header_value = HeaderValue::from_str(value).map_err(|error| ConnectFailure {
            reason: format!("invalid MCP header value for `{name}`: {error}"),
            config_error: true,
        })?;
        config.custom_headers.insert(header_name, header_value);
    }
    apply_http_auth_headers(&mut config, want)?;
    let transport = StreamableHttpClientTransport::with_client(client, config);
    Box::pin(handler.connect(transport)).await.map_err(|error| {
        let reason = error.to_string();
        ConnectFailure {
            config_error: is_config_error_message(&reason),
            reason,
        }
    })
}

fn apply_http_auth_headers(
    config: &mut StreamableHttpClientTransportConfig,
    want: &Desired,
) -> Result<(), ConnectFailure> {
    let Some(secret) = want
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
    else {
        return Ok(());
    };
    let auth_header = want.auth_header.trim();
    if auth_header.is_empty() || auth_header.eq_ignore_ascii_case("authorization") {
        if let Some(bearer) = secret
            .strip_prefix("Bearer ")
            .or_else(|| secret.strip_prefix("bearer "))
            .map(str::trim)
        {
            config.auth_header = Some(bearer.to_owned());
        } else if secret.contains(char::is_whitespace) {
            let header_value = HeaderValue::from_str(secret).map_err(|error| ConnectFailure {
                reason: format!("invalid MCP credential header: {error}"),
                config_error: true,
            })?;
            config.custom_headers.insert(AUTHORIZATION, header_value);
        } else {
            config.auth_header = Some(secret.to_owned());
        }
        return Ok(());
    }
    let header_name =
        HeaderName::from_bytes(auth_header.as_bytes()).map_err(|error| ConnectFailure {
            reason: format!("invalid MCP auth_header `{auth_header}`: {error}"),
            config_error: true,
        })?;
    let header_value = HeaderValue::from_str(secret).map_err(|error| ConnectFailure {
        reason: format!("invalid MCP credential value: {error}"),
        config_error: true,
    })?;
    config.custom_headers.insert(header_name, header_value);
    Ok(())
}

/// The model-facing name for one server's tool: `mcp_<server>_<tool>`.
pub fn exposed_name(label: &str, id: &str, tool: &str) -> Option<String> {
    let mut name = String::from("mcp_");
    if !push_allowed(&mut name, label) {
        let short: String = id.chars().take(8).collect();
        if !push_allowed(&mut name, &short) {
            return None;
        }
    }
    name.push('_');
    if !push_allowed(&mut name, tool) || name.len() > 64 {
        return None;
    }
    Some(name)
}

fn push_allowed(out: &mut String, text: &str) -> bool {
    if text.is_empty()
        || !text.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return false;
    }
    out.push_str(text);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_desired() -> Desired {
        Desired {
            revision: 1,
            credential_generation: None,
            name: "srv".into(),
            command: String::new(),
            args: Vec::new(),
            cwd: String::new(),
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            auth_header: String::new(),
            secret_env_var: String::new(),
            endpoint: String::new(),
            secret: None,
            allowed: Vec::new(),
        }
    }

    #[test]
    fn retry_jitter_stays_inside_the_backoff_window() {
        for (attempt, upper) in [(1, 1), (2, 2), (6, 32), (7, 60), (10, 60), (u32::MAX, 60)] {
            let upper = Duration::from_secs(upper);
            for _ in 0..32 {
                let delay = backoff_delay(attempt);
                assert!(
                    (upper / 2..=upper).contains(&delay),
                    "attempt {attempt}: {delay:?}"
                );
            }
        }
    }

    #[test]
    fn config_errors_and_exposed_names_behave_deterministically() {
        assert!(is_config_error_message("HTTP 401 Unauthorized"));
        assert!(is_config_error_message("ENOENT: no such file"));
        assert!(!is_config_error_message("connection reset by peer"));

        assert_eq!(
            exposed_name("notes", "id-1234", "search").as_deref(),
            Some("mcp_notes_search")
        );
        assert_eq!(
            exposed_name("悪い名前", "abcd1234-xyz", "search").as_deref(),
            Some("mcp_abcd1234_search")
        );
        assert_eq!(exposed_name("notes", "id", "bad.name"), None);
    }

    #[test]
    fn http_auth_headers_apply_bearer_custom_scheme_and_named_header() {
        let mut d = empty_desired();
        d.secret = Some("Bearer tok-1".into());
        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        apply_http_auth_headers(&mut cfg, &d).unwrap();
        assert_eq!(cfg.auth_header.as_deref(), Some("tok-1"));

        d.secret = Some("Token abc".into());
        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        apply_http_auth_headers(&mut cfg, &d).unwrap();
        assert_eq!(cfg.custom_headers[&AUTHORIZATION], "Token abc");

        d.auth_header = "x-api-key".into();
        d.secret = Some("k-1".into());
        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        apply_http_auth_headers(&mut cfg, &d).unwrap();
        assert_eq!(
            cfg.custom_headers[&HeaderName::from_static("x-api-key")],
            "k-1"
        );
    }

    #[tokio::test]
    async fn open_rejects_empty_command_and_endpoint_as_config_error() {
        let d = empty_desired();
        let Err(err) = open(
            &d,
            rig_agent::tool::server::ToolServer::new().run(),
            reqwest::Client::new(),
        )
        .await
        else {
            panic!("expected empty command and endpoint to fail");
        };
        assert!(err.config_error);
        assert!(err.reason.contains("neither a command nor an endpoint"));
    }
}
