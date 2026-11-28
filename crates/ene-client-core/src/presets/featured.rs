//! Fixed presets for the add panel and first-run setup.

use super::preset_endpoint;
use crate::domain::{
    Connection, ConnectionConfig, ConnectionInput, EmbeddingConnection, SttConnection,
    TtsConnection,
};
use ene_catalog::SAKURA_AI_ENDPOINT;

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1";
const STT_LABEL: &str = "音声認識（STT）接続の追加";
const EMBEDDING_LABEL: &str = "埋め込み（Embedding）接続の追加";

/// Featured connection choices exposed by the desktop's add panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeaturedPreset {
    /// Local VOICEVOX synthesis.
    Voicevox,
    /// Sakura AI Engine synthesis.
    SakuraTts,
    /// OpenAI-compatible synthesis.
    OpenAiTts,
    /// Sakura AI Engine transcription.
    SakuraStt,
    /// OpenAI transcription.
    OpenAiStt,
    /// Local Whisper transcription.
    WhisperStt,
    /// Sakura AI Engine embeddings.
    SakuraEmbedding,
    /// OpenAI embeddings.
    OpenAiEmbedding,
}

impl FeaturedPreset {
    /// Builds the settings input, its action label and the connection to probe after saving.
    pub fn prepare(
        self,
        connections: &[Connection],
    ) -> (&'static str, ConnectionInput, Option<String>) {
        use FeaturedPreset::*;
        let (fixed_id, name, label, config) = match self {
            Voicevox => (
                Some("voicevox"),
                "VOICEVOX（ローカル）",
                "VOICEVOX接続の追加",
                tts(
                    "voicevox",
                    crate::VOICEVOX_DEFAULT_ENDPOINT,
                    r#"{"speaker":3}"#,
                    "3",
                ),
            ),
            SakuraTts => (
                Some("sakura-tts"),
                "さくらのAI Engine TTS",
                "さくらのAI Engine TTS接続の追加",
                tts(
                    "sakura",
                    SAKURA_AI_ENDPOINT,
                    serde_json::json!({
                        "protocol": "sakura", "model": "zundamon", "voice": "normal",
                        "response_format": "wav",
                    })
                    .to_string(),
                    "zundamon/normal",
                ),
            ),
            OpenAiTts => (
                None,
                "OpenAI互換 TTS",
                "TTS接続の追加",
                tts(
                    "openai",
                    OPENAI_ENDPOINT,
                    serde_json::json!({"protocol": "openai", "model": "tts-1", "voice": "alloy"})
                        .to_string(),
                    "alloy",
                ),
            ),
            SakuraStt => (
                Some("sakura-stt"),
                "さくらのAI Engine STT",
                STT_LABEL,
                stt("sakura", SAKURA_AI_ENDPOINT, "whisper-large-v3-turbo"),
            ),
            OpenAiStt => (
                None,
                "OpenAI 音声認識",
                STT_LABEL,
                stt("openai", OPENAI_ENDPOINT, "gpt-4o-mini-transcribe"),
            ),
            WhisperStt => (
                None,
                "ローカル Whisper",
                STT_LABEL,
                stt("openai", "http://127.0.0.1:8080/v1", "whisper-1"),
            ),
            SakuraEmbedding => (
                Some("sakura-embedding"),
                "さくらのAI Engine Embedding",
                EMBEDDING_LABEL,
                embedding("sakura", SAKURA_AI_ENDPOINT, "multilingual-e5-large", 1024),
            ),
            OpenAiEmbedding => (
                Some("openai-embedding"),
                "OpenAI Embedding",
                EMBEDDING_LABEL,
                embedding("openai", OPENAI_ENDPOINT, "text-embedding-3-small", 1536),
            ),
        };
        let expected_revision = fixed_id
            .and_then(|id| connections.iter().find(|c| c.id == id))
            .map_or(0, |connection| connection.revision);
        let input = ConnectionInput {
            id: fixed_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
            name: name.into(),
            enabled: true,
            expected_revision,
            config: Some(config),
        };
        let probe = (self == Voicevox).then(|| input.id.clone());
        (label, input, probe)
    }
}

fn tts(adapter: &str, endpoint: &str, options: impl Into<String>, voice: &str) -> ConnectionConfig {
    ConnectionConfig::Tts(TtsConnection {
        endpoint: preset_endpoint(adapter, endpoint, options),
        voice_ids: vec![voice.into()],
    })
}

fn stt(adapter: &str, endpoint: &str, model: &str) -> ConnectionConfig {
    let language = "ja";
    let options = serde_json::json!({"protocol": adapter, "model": model, "language": language});
    ConnectionConfig::Stt(SttConnection {
        endpoint: preset_endpoint(adapter, endpoint, options.to_string()),
        language_hints: vec![language.into()],
    })
}

fn embedding(adapter: &str, endpoint: &str, model: &str, dimensions: u32) -> ConnectionConfig {
    ConnectionConfig::Embedding(EmbeddingConnection {
        endpoint: preset_endpoint(adapter, endpoint, "{}"),
        model_id: model.into(),
        dimensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_voicevox_probes_and_fixed_ids_keep_their_revisions() {
        use FeaturedPreset::*;
        for (preset, fixed) in [
            (Voicevox, Some("voicevox")),
            (SakuraTts, Some("sakura-tts")),
            (OpenAiTts, None),
            (SakuraStt, Some("sakura-stt")),
            (OpenAiStt, None),
            (WhisperStt, None),
            (SakuraEmbedding, Some("sakura-embedding")),
            (OpenAiEmbedding, Some("openai-embedding")),
        ] {
            let (_, new, _) = preset.prepare(&[]);
            assert_eq!(new.expected_revision, 0);
            let existing = Connection {
                id: new.id.clone(),
                revision: 7,
                ..Default::default()
            };
            let (_, saved, probe) = preset.prepare(&[existing]);
            if let Some(id) = fixed {
                assert_eq!(saved.id, id);
                assert_eq!(saved.expected_revision, 7);
            } else {
                assert_ne!(saved.id, new.id);
                assert_eq!(saved.expected_revision, 0);
                assert!(uuid::Uuid::parse_str(&saved.id).is_ok());
            }
            assert_eq!(probe.as_deref(), (preset == Voicevox).then_some("voicevox"));
        }
    }
}
