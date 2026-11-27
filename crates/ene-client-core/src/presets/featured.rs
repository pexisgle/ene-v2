//! Fixed presets for the add panel and first-run setup.

use super::{
    EmbeddingPresetSpec, SttPresetSpec, build_embedding_preset_input,
    build_openai_tts_preset_input, build_sakura_tts_preset_input, build_stt_preset_input,
    build_voicevox_preset_input,
};
use crate::domain::{Connection, ConnectionInput};

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
        let stt = |fixed_id, adapter, name: &str, endpoint: &str, model: &str| {
            let spec = SttPresetSpec {
                fixed_id,
                adapter,
                name: name.into(),
                endpoint: endpoint.into(),
                model: model.into(),
                language: "ja".into(),
            };
            (
                "音声認識（STT）接続の追加",
                build_stt_preset_input(connections, spec),
            )
        };
        let (label, input) = match self {
            Voicevox => (
                "VOICEVOX接続の追加",
                build_voicevox_preset_input(
                    connections
                        .iter()
                        .find(|c| c.id == "voicevox")
                        .map_or(0, |c| c.revision),
                    3,
                    None,
                ),
            ),
            SakuraTts => (
                "さくらのAI Engine TTS接続の追加",
                build_sakura_tts_preset_input(connections),
            ),
            OpenAiTts => (
                "TTS接続の追加",
                build_openai_tts_preset_input("https://api.openai.com/v1", "tts-1", "alloy"),
            ),
            SakuraStt => stt(
                Some("sakura-stt"),
                "sakura",
                "さくらのAI Engine STT",
                ene_catalog::SAKURA_AI_ENDPOINT,
                "whisper-large-v3-turbo",
            ),
            OpenAiStt => stt(
                None,
                "openai",
                "OpenAI 音声認識",
                "https://api.openai.com/v1",
                "gpt-4o-mini-transcribe",
            ),
            WhisperStt => stt(
                None,
                "openai",
                "ローカル Whisper",
                "http://127.0.0.1:8080/v1",
                "whisper-1",
            ),
            SakuraEmbedding | OpenAiEmbedding => {
                let spec = if self == SakuraEmbedding {
                    EmbeddingPresetSpec {
                        id: "sakura-embedding",
                        name: "さくらのAI Engine Embedding",
                        adapter: "sakura",
                        endpoint: ene_catalog::SAKURA_AI_ENDPOINT,
                        model_id: "multilingual-e5-large",
                        dimensions: 1024,
                    }
                } else {
                    EmbeddingPresetSpec {
                        id: "openai-embedding",
                        name: "OpenAI Embedding",
                        adapter: "openai",
                        endpoint: "https://api.openai.com/v1",
                        model_id: "text-embedding-3-small",
                        dimensions: 1536,
                    }
                };
                (
                    "埋め込み（Embedding）接続の追加",
                    build_embedding_preset_input(connections, &spec),
                )
            }
        };
        let probe = (self == Voicevox).then(|| input.id.clone());
        (label, input, probe)
    }
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
