//! Validation and credential receiver identity for MCP connections.
use super::McpConnection;
use crate::error::DomainError;
use http::header::{HeaderName, HeaderValue};

/// Recognizes an npx executable or standard wrapper, with or without a path.
pub fn is_npx_command(command: &str) -> bool {
    let base = command
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    ["npx", "npx.cmd", "npx.ps1", "npx.exe"]
        .into_iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

impl McpConnection {
    /// Whether both configurations would use a credential for the same receiver.
    pub fn same_credential_receiver(&self, other: &Self) -> bool {
        let command = self.command.trim();
        let other_command = other.command.trim();
        if command.is_empty() && other_command.is_empty() {
            let auth_header = |value: &str| {
                let value = value.trim();
                if value.is_empty() {
                    "authorization".to_owned()
                } else {
                    value.to_ascii_lowercase()
                }
            };
            self.endpoint.endpoint.trim() == other.endpoint.endpoint.trim()
                && auth_header(&self.auth_header) == auth_header(&other.auth_header)
                && self.headers == other.headers
        } else {
            command == other_command
                && self.args == other.args
                && self.cwd.trim() == other.cwd.trim()
                && self.env == other.env
                && self.secret_env_var.trim() == other.secret_env_var.trim()
        }
    }
}

pub(super) fn validate_mcp(config: &McpConnection) -> Result<(), DomainError> {
    if config.command.trim().is_empty() && config.endpoint.endpoint.trim().is_empty() {
        return Err(DomainError::Invalid(
            "MCP connections need a command or an endpoint".into(),
        ));
    }
    if config.cwd.chars().count() > 2_048 || config.cwd.contains('\0') {
        return Err(DomainError::Invalid(
            "MCP working directory is invalid".into(),
        ));
    }
    if config.env.len() > 64 || config.headers.len() > 64 {
        return Err(DomainError::Invalid(
            "too many MCP environment variables or headers".into(),
        ));
    }
    for (key, value) in &config.env {
        if key.trim().is_empty() || key.contains(['=', '\0', '\r', '\n']) || value.contains('\0') {
            return Err(DomainError::Invalid(
                "MCP environment variable entry is invalid".into(),
            ));
        }
    }
    for (name, value) in &config.headers {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || HeaderName::from_bytes(trimmed.as_bytes()).is_err()
            || HeaderValue::from_str(value).is_err()
        {
            return Err(DomainError::Invalid(format!("invalid MCP header `{name}`")));
        }
    }
    let auth_header = config.auth_header.trim();
    if !auth_header.is_empty() && HeaderName::from_bytes(auth_header.as_bytes()).is_err() {
        return Err(DomainError::Invalid(
            "MCP auth_header must be a valid HTTP header name".into(),
        ));
    }
    let secret_env_var = config.secret_env_var.trim();
    if !secret_env_var.is_empty() && !is_valid_env_name(secret_env_var) {
        return Err(DomainError::Invalid(
            "MCP secret_env_var must be a valid environment variable name".into(),
        ));
    }
    Ok(())
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && name.len() <= 128
}
