//! Resolution of the connection endpoint options into an engine's settings:
//! the wire protocol, request paths, and the default model, voice, and language.

use ene_catalog::is_sakura_tts_model_id;
use ene_voice::SetupError;

use serde::Deserialize;

#[derive(Deserialize, Default)]
pub(crate) struct SpeechOptions {
    pub(crate) protocol: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) voice: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) response_format: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) audio_query_path: Option<String>,
    pub(crate) synthesis_path: Option<String>,
    pub(crate) speaker: Option<u64>,
    pub(crate) speed: Option<f64>,
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    OpenAiTts,
    Voicevox,
    OpenAiStt,
    Raw,
}

pub(crate) fn resolve_protocol(
    tts: bool,
    adapter: &str,
    protocol_opt: Option<&str>,
    label: &str,
) -> Result<Protocol, SetupError> {
    let trimmed = protocol_opt.map(str::trim).filter(|s| !s.is_empty());
    match (tts, trimmed) {
        (true, None) if adapter == "voicevox" => Ok(Protocol::Voicevox),
        (true, None | Some("openai" | "sakura")) => Ok(Protocol::OpenAiTts),
        (true, Some("voicevox")) => Ok(Protocol::Voicevox),
        (false, None | Some("openai" | "sakura")) => Ok(Protocol::OpenAiStt),
        (false, Some("raw")) => Ok(Protocol::Raw),
        (_, Some(other)) => Err(SetupError::Unsupported(format!(
            "the {label} adapter protocol `{other}` is not supported"
        ))),
    }
}

pub(crate) fn split_sakura_voice_defaults(
    default_voice: Option<&str>,
    sakura: bool,
) -> (Option<String>, Option<String>) {
    match default_voice {
        Some(raw) if sakura => {
            let trimmed = raw.trim();
            if let Some((m, v)) = trimmed.split_once('/') {
                let m = m.trim();
                let v = v.trim();
                (
                    (!m.is_empty()).then(|| m.to_owned()),
                    (!v.is_empty()).then(|| v.to_owned()),
                )
            } else if !trimmed.is_empty() && is_sakura_tts_model_id(trimmed) {
                (Some(trimmed.to_owned()), None)
            } else {
                (None, (!trimmed.is_empty()).then(|| trimmed.to_owned()))
            }
        }
        _ => (None, default_voice.map(str::to_owned)),
    }
}

pub(crate) fn default_engine_model(tts: bool, sakura: bool) -> String {
    if !tts {
        if sakura {
            "whisper-large-v3-turbo"
        } else {
            "whisper-1"
        }
    } else if sakura {
        "zundamon"
    } else {
        "tts-1"
    }
    .into()
}

pub(crate) fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

pub(crate) fn endpoint_path(value: Option<String>, default: &str) -> String {
    nonempty(value).map_or_else(
        || default.to_owned(),
        |path| {
            if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            }
        },
    )
}
