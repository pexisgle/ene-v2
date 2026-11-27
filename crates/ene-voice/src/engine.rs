//! Speech engine ports and connection selection.

use crate::error::{BoxFuture, HttpResult, SetupError};
use ene_domain::settings::{Connection, ConnectionKind};
use ene_mind::Companion;

pub trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>>;
}

pub trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>>;
}

pub fn parse_binding(companion: &Companion) -> ene_domain::VoiceBinding {
    let mut binding =
        serde_json::from_value::<ene_domain::VoiceBinding>(companion.voice.json.clone())
            .unwrap_or_default();
    if binding
        .voice_id
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        binding.voice_id = companion.voice_identifier.clone();
    }
    binding
}

/// Selects the first enabled connection matching `kind`, reporting `Disabled` if only disabled
/// connections exist or `Missing` otherwise.
pub fn select_connection(
    connections: &[Connection],
    kind: ConnectionKind,
) -> Result<Connection, SetupError> {
    let mut first_disabled = None;
    for connection in connections {
        if connection.kind != kind {
            continue;
        }
        if connection.enabled {
            return Ok(connection.clone());
        }
        if first_disabled.is_none() {
            first_disabled = Some(connection);
        }
    }
    if let Some(connection) = first_disabled {
        return Err(SetupError::Disabled(format!(
            "the {} connection `{}` is disabled",
            kind.as_str(),
            connection.id
        )));
    }
    Err(SetupError::Missing)
}

/// Selects the connection with `id` and `kind`, failing with `Missing` or `Disabled`.
pub fn select_connection_by_id(
    connections: &[Connection],
    kind: ConnectionKind,
    id: &str,
) -> Result<Connection, SetupError> {
    let Some(connection) = connections
        .iter()
        .find(|connection| connection.id.as_str() == id && connection.kind == kind)
    else {
        return Err(SetupError::Missing);
    };
    if !connection.enabled {
        return Err(SetupError::Disabled(format!(
            "the {} connection `{}` is disabled",
            kind.as_str(),
            connection.id
        )));
    }
    Ok(connection.clone())
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
