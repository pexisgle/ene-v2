//! Pure vendor, endpoint, voice, and search connection catalog helpers.
pub mod connections;
pub mod vendors;
pub mod voice;

pub use connections::{
    SearchConnectionSpec, can_start_chat, llm_endpoint, llm_provider_input, needs_key,
    search_adapter_of, search_adapter_requires_key, search_connection_input,
};
pub use vendors::{
    GOOGLE_AGENT_PLATFORM, GOOGLE_AGENT_PLATFORM_DEFAULT_MODEL, GOOGLE_AGENT_PLATFORM_ENDPOINT,
    GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST, SAKURA_AI_API_KEY_ENV, SAKURA_AI_ENDPOINT,
    SAKURA_CHAT_MODELS, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES,
    SAKURA_VISION_MODELS, default_embedding_dimensions, default_models_for_adapter,
    is_non_chat_sakura_model, is_sakura_conn, is_sakura_connection, is_sakura_endpoint,
    is_sakura_tts_model_id,
};
pub use voice::{
    OPENAI_TTS_VOICES, active_stt_in_connections, active_tts_in_connections,
    apply_stt_model_to_config, apply_tts_voice_to_config, build_stt_assignment_inputs,
    build_tts_assignment_inputs, build_voice_assignment_inputs, disable_connection_input,
    is_local_voicevox_conn, parse_voice_resource, stt_model_of_connection, tts_voice_of_connection,
};
