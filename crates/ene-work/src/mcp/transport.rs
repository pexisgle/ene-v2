//! ECS-free MCP transport connection opening, backoff calculation, and tool naming helpers.

use backon::{BackoffBuilder as _, ExponentialBuilder};
use ene_domain::settings::McpConnection as McpConfig;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use rig_agent::tool::server::ToolServerHandle;
use rig_rmcp::{McpClientError, McpClientHandler, McpConnection};
use rmcp::{
    ServiceError,
    model::ClientConfig,
    service::ClientInitializeError,
    transport::{
        DynamicTransportError,
        child_process::TokioChildProcess,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig, StreamableHttpError,
        },
    },
};
use std::time::Duration;

#[cfg(windows)]
use super::npx::CREATE_NO_WINDOW;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectFailure {
    Configuration(String),
    Transient(String),
}

impl ConnectFailure {
    pub fn reason(&self) -> &str {
        match self {
            Self::Configuration(reason) | Self::Transient(reason) => reason,
        }
    }
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

fn connect_failure(error: McpClientError) -> ConnectFailure {
    match error {
        McpClientError::Connection(error) => handshake_failure(&error),
        McpClientError::Request(ServiceError::TransportSend(error)) => transport_failure(&error),
        McpClientError::Timeout(_) => {
            ConnectFailure::Transient("MCP tool discovery timed out".into())
        }
        McpClientError::Request(_) => ConnectFailure::Transient("MCP tool discovery failed".into()),
    }
}

fn handshake_failure(error: &ClientInitializeError) -> ConnectFailure {
    if error.is_authorization_required() {
        return ConnectFailure::Configuration("MCP authorization was rejected".into());
    }
    match error {
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => handshake_failure(fallback),
        ClientInitializeError::TransportError { error, .. } => transport_failure(error),
        ClientInitializeError::NoCompatibleProtocolVersion { .. } => {
            ConnectFailure::Configuration("no compatible MCP protocol version".into())
        }
        _ => ConnectFailure::Transient("MCP initialization did not complete".into()),
    }
}

fn transport_failure(error: &DynamicTransportError) -> ConnectFailure {
    use ConnectFailure::{Configuration, Transient};
    match error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    {
        Some(StreamableHttpError::AuthRequired(_) | StreamableHttpError::InsufficientScope(_)) => {
            Configuration("MCP authorization was rejected".into())
        }
        Some(StreamableHttpError::ReservedHeaderConflict(_)) => {
            Configuration("an MCP header conflicts with a protocol header".into())
        }
        Some(StreamableHttpError::HttpStatus(error)) => {
            Transient(format!("MCP server returned HTTP {}", error.status))
        }
        Some(StreamableHttpError::UnexpectedServerResponse(message)) => {
            // This locked backend puts an unchallenged HTTP status before the peer body.
            // Only the status token may cross the diagnostic boundary.
            match message
                .strip_prefix("HTTP ")
                .and_then(|rest| rest.split_once(' '))
            {
                Some(("401" | "403", _)) => Configuration("MCP authorization was rejected".into()),
                _ => Transient("MCP server returned an unexpected response".into()),
            }
        }
        Some(StreamableHttpError::Client(error)) if error.is_builder() => {
            Configuration("MCP HTTP request configuration is invalid".into())
        }
        _ => Transient("MCP transport did not complete the request".into()),
    }
}

/// Opens either a stdio child-process MCP server or a streamable HTTP MCP server.
pub async fn open(
    want: &McpConfig,
    secret: Option<&str>,
    registry: ToolServerHandle,
    http_client: reqwest::Client,
) -> Result<McpConnection, ConnectFailure> {
    if !want.command.trim().is_empty() {
        Box::pin(open_stdio(want, secret, registry)).await
    } else if !want.endpoint.endpoint.trim().is_empty() {
        Box::pin(open_http(want, secret, registry, http_client)).await
    } else {
        Err(ConnectFailure::Configuration(
            "the MCP connection names neither a command nor an endpoint".into(),
        ))
    }
}

async fn open_stdio(
    want: &McpConfig,
    secret: Option<&str>,
    registry: ToolServerHandle,
) -> Result<McpConnection, ConnectFailure> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    #[cfg(windows)]
    let mut command = if super::npx::is_npx_command(want.command.trim()) {
        super::npx::build_command(want).await?
    } else {
        let mut command = tokio::process::Command::new(want.command.trim());
        command.args(&want.args);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = tokio::process::Command::new(want.command.trim());
        command.args(&want.args);
        command
    };
    if !want.cwd.trim().is_empty() {
        command.current_dir(want.cwd.trim());
    }
    if !want.env.is_empty() {
        command.envs(&want.env);
    }
    if let Some(secret) = secret.filter(|s| !s.is_empty())
        && !want.secret_env_var.trim().is_empty()
    {
        command.env(want.secret_env_var.trim(), secret);
    }
    let mut wrap = process_wrap::tokio::CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        // JobObject::pre_spawn sets the process creation flags from whatever
        // `CreationFlags` wrapper is registered, and zero when none is. The raw
        // `Command::creation_flags` a caller set would be overwritten, so the
        // suppression has to be registered here to survive.
        wrap.wrap(process_wrap::tokio::CreationFlags(
            windows::Win32::System::Threading::PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW),
        ));
        wrap.wrap(process_wrap::tokio::JobObject);
    }
    wrap.wrap(process_wrap::tokio::KillOnDrop);
    let transport = TokioChildProcess::new(wrap).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::InvalidInput => {
            ConnectFailure::Configuration(format!("MCP process could not start: {error}"))
        }
        _ => ConnectFailure::Transient(format!("MCP process could not start: {error}")),
    })?;
    Box::pin(handler.connect(transport))
        .await
        .map_err(connect_failure)
}

async fn open_http(
    want: &McpConfig,
    secret: Option<&str>,
    registry: ToolServerHandle,
    client: reqwest::Client,
) -> Result<McpConnection, ConnectFailure> {
    let handler = McpClientHandler::new(ClientConfig::default(), registry);
    let mut config = StreamableHttpClientTransportConfig::with_uri(want.endpoint.endpoint.trim());
    for (name, value) in &want.headers {
        let header_name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|error| {
            ConnectFailure::Configuration(format!("invalid MCP header name: {error}"))
        })?;
        let header_value = sensitive_header(value)?;
        config.custom_headers.insert(header_name, header_value);
    }
    apply_http_auth_headers(&mut config, &want.auth_header, secret)?;
    let transport = StreamableHttpClientTransport::with_client(client, config);
    Box::pin(handler.connect(transport))
        .await
        .map_err(connect_failure)
}

fn apply_http_auth_headers(
    config: &mut StreamableHttpClientTransportConfig,
    auth_header: &str,
    secret: Option<&str>,
) -> Result<(), ConnectFailure> {
    let Some(secret) = secret.map(str::trim).filter(|secret| !secret.is_empty()) else {
        return Ok(());
    };
    let auth_header = auth_header.trim();
    if auth_header.is_empty() || auth_header.eq_ignore_ascii_case("authorization") {
        config.custom_headers.remove(&AUTHORIZATION);
        if let Some(bearer) = secret
            .strip_prefix("Bearer ")
            .or_else(|| secret.strip_prefix("bearer "))
            .map(str::trim)
        {
            config.auth_header = Some(bearer.to_owned());
        } else if secret.contains(char::is_whitespace) {
            config
                .custom_headers
                .insert(AUTHORIZATION, sensitive_header(secret)?);
        } else {
            config.auth_header = Some(secret.to_owned());
        }
        return Ok(());
    }
    let name = HeaderName::from_bytes(auth_header.as_bytes()).map_err(|error| {
        ConnectFailure::Configuration(format!("invalid MCP auth header: {error}"))
    })?;
    config
        .custom_headers
        .insert(name, sensitive_header(secret)?);
    Ok(())
}

fn sensitive_header(value: &str) -> Result<HeaderValue, ConnectFailure> {
    let mut header = HeaderValue::from_str(value).map_err(|error| {
        ConnectFailure::Configuration(format!("invalid MCP header value: {error}"))
    })?;
    header.set_sensitive(true);
    Ok(header)
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
    fn exposed_names_behave_deterministically() {
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
        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        cfg.custom_headers
            .insert(AUTHORIZATION, HeaderValue::from_static("stale"));
        apply_http_auth_headers(&mut cfg, "", Some("Bearer tok-1")).unwrap();
        assert!(!cfg.custom_headers.contains_key(&AUTHORIZATION));
        assert_eq!(cfg.auth_header.as_deref(), Some("tok-1"));

        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        apply_http_auth_headers(&mut cfg, "", Some("Token abc")).unwrap();
        assert_eq!(cfg.custom_headers[&AUTHORIZATION], "Token abc");

        let mut cfg = StreamableHttpClientTransportConfig::with_uri("http://127.0.0.1");
        apply_http_auth_headers(&mut cfg, "x-api-key", Some("k-1")).unwrap();
        assert_eq!(
            cfg.custom_headers[&HeaderName::from_static("x-api-key")],
            "k-1"
        );
    }

    #[tokio::test]
    async fn open_rejects_empty_command_and_endpoint_as_config_error() {
        let Err(err) = open(
            &McpConfig::default(),
            None,
            rig_agent::tool::server::ToolServer::new().run(),
            reqwest::Client::new(),
        )
        .await
        else {
            panic!("expected empty command and endpoint to fail");
        };
        assert!(matches!(err, ConnectFailure::Configuration(_)));
        assert!(err.reason().contains("neither a command nor an endpoint"));
    }
}
