//! Voice (STT/TTS) resource name parsing and model catalogs.
//!
//! Connection mutation helpers live in `ene-client-core`, which owns the
//! transport-agnostic `Connection` model; this crate stays wire-free.

pub const OPENAI_TTS_VOICES: &[&str] = &[
    "alloy", "ash", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer",
];

/// The default address of the local VOICEVOX process.
pub const VOICEVOX_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:50021";

/// Resolves a local VOICEVOX HTTP endpoint to its bind host and port.
pub fn voicevox_local_address(endpoint: &str) -> Option<(String, u16)> {
    let endpoint = url::Url::parse(endpoint.trim()).ok()?;
    if endpoint.scheme() != "http" {
        return None;
    }
    let host = match endpoint.host()? {
        url::Host::Ipv4(ip) if ip.is_loopback() => ip.to_string(),
        url::Host::Ipv6(ip) if ip.is_loopback() => ip.to_string(),
        url::Host::Domain(name) if name.eq_ignore_ascii_case("localhost") => "127.0.0.1".into(),
        _ => return None,
    };
    Some((host, endpoint.port().unwrap_or(50021)))
}

/// Splits a `id:name` voice resource reference, falling back to using the
/// trimmed input for both halves.
pub fn parse_voice_resource(raw: &str) -> (String, String) {
    let trimmed = raw.trim();
    if let Some((id, name)) = trimmed.split_once(':') {
        let id = id.trim();
        let name = name.trim();
        if !id.is_empty() && !name.is_empty() {
            return (id.to_owned(), name.to_owned());
        }
    }
    (trimmed.to_owned(), trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_resources_split_on_the_first_colon_and_fall_back_to_the_raw_name() {
        assert_eq!(
            parse_voice_resource("voicevox:四国めたん"),
            ("voicevox".into(), "四国めたん".into())
        );
        assert_eq!(
            parse_voice_resource("  bare-name  "),
            ("bare-name".into(), "bare-name".into())
        );
        assert_eq!(
            parse_voice_resource(":only-name"),
            (":only-name".into(), ":only-name".into())
        );
    }
}
