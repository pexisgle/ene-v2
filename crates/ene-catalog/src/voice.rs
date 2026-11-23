//! Voice (STT/TTS) resource name parsing and model catalogs.
//!
//! Connection mutation helpers live in `ene-client-core`, which owns the
//! transport-agnostic `Connection` model; this crate stays wire-free.

pub const OPENAI_TTS_VOICES: &[&str] = &[
    "alloy", "ash", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer",
];

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
