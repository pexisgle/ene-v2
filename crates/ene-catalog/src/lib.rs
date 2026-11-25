//! Pure vendor, endpoint, voice, and search connection catalog helpers.
//!
//! These are string/identifier facts shared by the host and the desktop client.
//! Anything that inspects or mutates a connection lives in `ene-client-core`,
//! which owns the transport-agnostic `Connection` model; this crate stays free
//! of the wire protocol crate.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
#![warn(clippy::needless_pass_by_value)]
pub mod connections;
pub mod vendors;
pub mod voice;

pub use connections::search_adapter_requires_key;
pub use vendors::{
    GOOGLE_AGENT_PLATFORM, GOOGLE_AGENT_PLATFORM_DEFAULT_MODEL, GOOGLE_AGENT_PLATFORM_ENDPOINT,
    GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST, SAKURA_AI_API_KEY_ENV, SAKURA_AI_ENDPOINT,
    SAKURA_CHAT_MODELS, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES,
    SAKURA_VISION_MODELS, default_embedding_dimensions, default_models_for_adapter,
    is_non_chat_sakura_model, is_sakura_connection, is_sakura_endpoint, is_sakura_tts_model_id,
};
pub use voice::{OPENAI_TTS_VOICES, parse_voice_resource};
