//! Vendor endpoints, known model defaults, and adapter predicates shared
//! between `ene-host` and `ene-desktop`.

pub const GOOGLE_AGENT_PLATFORM: &str = "google-agent-platform";
pub const GOOGLE_AGENT_PLATFORM_ENDPOINT: &str = "https://aiplatform.googleapis.com";
pub const GOOGLE_AGENT_PLATFORM_DEFAULT_MODEL: &str = "gemini-3.8-flash";
pub const GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST: &str = "verified_model_list";

pub const SAKURA_AI_ENDPOINT: &str = "https://api.ai.sakura.ad.jp/v1";
pub const SAKURA_AI_API_KEY_ENV: &str = "SAKURA_AI_API_KEY";

pub const SAKURA_CHAT_MODELS: &[&str] = &[
    "gpt-oss-120b",
    "llm-jp-3.1-8x13b-instruct4",
    "preview/Qwen3.6-35B-A3B",
    "preview/gemma-4-31B-it",
    "preview/Kimi-K2.6",
    "preview/Kimi-K2.7-Code",
    "preview/Qwen3-VL-30B-A3B-Instruct",
    "preview/GLM-5.3-Flash",
    "preview/DeepSeek-V4.1-Flash-1017",
    "preview/Phi-4-mini-instruct-cpu",
    "preview/Qwen3-0.6B-cpu",
];

// https://manual.sakura.ad.jp/cloud/ai-engine/chat-models.html
pub const SAKURA_VISION_MODELS: &[&str] = &[
    "preview/gemma-4-31B-it",
    "preview/Qwen3-VL-30B-A3B-Instruct",
    "preview/Qwen3.6-35B-A3B",
    "preview/Kimi-K2.7-Code",
    "preview/GLM-5.3-Flash",
    "preview/DeepSeek-V4.1-Flash-1017",
];

pub const SAKURA_STT_MODELS: &[&str] = &["whisper-large-v3-turbo"];

pub const SAKURA_EMBEDDING_MODELS: &[&str] =
    &["multilingual-e5-large", "preview/Qwen3-Embedding-4B-FP16"];

pub const SAKURA_TTS_VOICES: &[&str] = &[
    "zundamon/normal:VOICEVOX:ずんだもん（ノーマル）",
    "zundamon/amaama:VOICEVOX:ずんだもん（あまあま）",
    "zundamon/tsuntsun:VOICEVOX:ずんだもん（ツンツン）",
    "zundamon/sexy:VOICEVOX:ずんだもん（セクシー）",
    "zundamon/sasayaki:VOICEVOX:ずんだもん（ささやき）",
    "shikokumetan/normal:VOICEVOX:四国めたん（ノーマル）",
    "shikokumetan/amaama:VOICEVOX:四国めたん（あまあま）",
    "shikokumetan/tsuntsun:VOICEVOX:四国めたん（ツンツン）",
    "shikokumetan/sexy:VOICEVOX:四国めたん（セクシー）",
    "kasukabetsumugi/normal:VOICEVOX:春日部つむぎ（ノーマル）",
    "meimeihimari/normal:VOICEVOX:冥鳴ひまり（ノーマル）",
    "tohokuzunko/normal:VOICEVOX:東北ずん子（ノーマル）",
    "tohokukiritan/normal:VOICEVOX:東北きりたん（ノーマル）",
    "tohokuitako/normal:VOICEVOX:東北イタコ（ノーマル）",
    "ankomon/normal:VOICEVOX:あんこもん（ノーマル）",
];

pub fn is_sakura_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint.trim())
        .ok()
        .and_then(|parsed| {
            parsed
                .host_str()
                .map(|host| host.eq_ignore_ascii_case("api.ai.sakura.ad.jp"))
        })
        .unwrap_or(false)
}

/// A connection targets Sakura AI Engine when its adapter says so or its
/// endpoint host is Sakura's.
pub fn is_sakura_connection(adapter: &str, endpoint: &str) -> bool {
    adapter.trim().eq_ignore_ascii_case("sakura") || is_sakura_endpoint(endpoint)
}

pub fn is_sakura_tts_model_id(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "zundamon"
            | "shikokumetan"
            | "kasukabetsumugi"
            | "meimeihimari"
            | "tohokuzunko"
            | "tohokukiritan"
            | "tohokuitako"
            | "ankomon"
            | "zunko"
            | "kiritan"
            | "itako"
            | "himari"
    )
}

pub fn is_non_chat_sakura_model(id: &str) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    lower.contains("whisper")
        || lower.contains("transcribe")
        || lower.contains("embedding")
        || lower.contains("multilingual-e5")
        || is_sakura_tts_model_id(&lower)
}

pub fn known_embedding_dimensions(model_id: &str) -> Option<u32> {
    match model_id.trim() {
        "multilingual-e5-large" => Some(1024),
        "preview/Qwen3-Embedding-4B-FP16" | "Qwen3-Embedding-4B" | "Qwen3-Embedding-4B-FP16" => {
            Some(2560)
        }
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        _ => None,
    }
}

pub fn default_embedding_dimensions(model_id: &str) -> u32 {
    known_embedding_dimensions(model_id).unwrap_or(1536)
}

pub fn default_models_for_adapter(adapter: &str) -> &'static [&'static str] {
    match adapter.trim().to_ascii_lowercase().as_str() {
        "openai" | "openai-responses" => &["gpt-5.4-mini", "gpt-5.4", "gpt-4o-mini", "gpt-4o"],
        "anthropic" => &[
            "claude-sonnet-4-6",
            "claude-3-5-haiku-latest",
            "claude-sonnet-4-20250514",
        ],
        "gemini" | "google" => &["gemini-2.5-flash", "gemini-2.5-pro"],
        GOOGLE_AGENT_PLATFORM => &[GOOGLE_AGENT_PLATFORM_DEFAULT_MODEL],
        "openrouter" | "openrouter-wasm" => &[
            "openai/gpt-5.4-mini",
            "openai/gpt-4o-mini",
            "openai/gpt-4o",
            "anthropic/claude-3.5-haiku",
            "google/gemini-2.5-flash",
        ],
        "sakura" => SAKURA_CHAT_MODELS,
        "ollama" | "vllm" | "lmstudio" => &["qwen2.5:7b"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sakura_predicates_and_embedding_dimensions_work() {
        assert!(is_sakura_connection("sakura", "https://custom.invalid/v1"));
        assert!(is_sakura_connection("openai", SAKURA_AI_ENDPOINT));
        assert!(!is_sakura_connection("openai", "https://api.openai.com/v1"));
        assert_eq!(default_embedding_dimensions("multilingual-e5-large"), 1024);
        assert_eq!(
            default_embedding_dimensions("preview/Qwen3-Embedding-4B-FP16"),
            2560
        );
        assert_eq!(default_embedding_dimensions("text-embedding-3-small"), 1536);
        assert_eq!(default_embedding_dimensions("text-embedding-3-large"), 3072);
    }
}
