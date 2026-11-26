//! Speech synthesis and recognition engine traits, selection helpers, and `HttpEngine`.

use crate::{
    engine_config::{
        Protocol, SpeechOptions, default_engine_model, endpoint_path, nonempty, resolve_protocol,
        split_sakura_voice_defaults,
    },
    engine_http::{post, read_response_body},
    error::{BoxFuture, HttpResult, SetupError, VoiceHttpError},
    pcm::{SAMPLE_RATE, parse_wav, resample_mono, wav_wrap},
};
use ene_catalog::{is_sakura_connection, is_sakura_tts_model_id};
use ene_domain::settings::{Connection, ConnectionConfig, ConnectionKind, Endpoint};
use ene_mind::Companion;

use serde::de::DeserializeOwned;
use std::{fmt, time::Duration};

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

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

/// Built-in HTTP engine supporting OpenAI TTS/STT, VOICEVOX, Sakura AI Engine, and raw STT.
pub struct HttpEngine {
    base: String,
    sakura: bool,
    protocol: Protocol,
    model: String,
    voice: String,
    language: Option<String>,
    response_format: String,
    path: String,
    query_path: String,
    synthesis_path: String,
    speaker: u32,
    speed: Option<f32>,
    pitch: Option<f32>,
    style: Option<String>,
    timeout: Duration,
    secret: Option<String>,
    client: reqwest::Client,
}

impl fmt::Debug for HttpEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpEngine")
            .field("base", &self.base)
            .field("sakura", &self.sakura)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("voice", &self.voice)
            .field("has_secret", &self.secret.is_some())
            .finish_non_exhaustive()
    }
}

impl HttpEngine {
    /// Constructs a built-in HTTP TTS or STT engine from resolved endpoint URL, secret, and client.
    pub fn new(
        connection: &Connection,
        resolved_base: &str,
        secret: Option<String>,
        kind: ConnectionKind,
        client: reqwest::Client,
    ) -> Result<Self, SetupError> {
        let tts = kind == ConnectionKind::Tts;
        let label = if tts { "TTS" } else { "STT" };
        let endpoint = connection.endpoint().clone();
        let resolved_base = resolved_base.trim().trim_end_matches('/').to_owned();
        url::Url::parse(&resolved_base).map_err(|_| {
            SetupError::Broken(format!("the {label} connection endpoint is not a URL"))
        })?;
        let options: SpeechOptions = parse_options(&endpoint, label)?;
        let protocol =
            resolve_protocol(tts, &endpoint.adapter, options.protocol.as_deref(), label)?;
        let sakura = is_sakura_connection(&endpoint.adapter, &resolved_base);
        let use_sakura_tts_v1 = protocol == Protocol::Voicevox
            && sakura
            && options.audio_query_path.is_none()
            && options.synthesis_path.is_none();
        let base = if use_sakura_tts_v1 {
            resolved_base.trim_end_matches("/v1").to_owned()
        } else {
            resolved_base
        };
        let default_voice = match &connection.config {
            ConnectionConfig::Tts(config) if tts => config.voice_ids.first().cloned(),
            _ => None,
        };
        let hints: &[String] = match &connection.config {
            ConnectionConfig::Stt(config) if !tts => &config.language_hints,
            _ => &[],
        };
        let (default_sakura_model, default_sakura_voice) =
            split_sakura_voice_defaults(default_voice.as_deref(), sakura);
        let default_speaker = default_voice
            .as_deref()
            .and_then(|v| v.trim().split(':').next())
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(3);
        let timeout = Duration::from_millis(
            options
                .timeout_ms
                .unwrap_or(if tts { 20_000 } else { 30_000 })
                .clamp(1_000, 300_000),
        );
        Ok(Self {
            base,
            sakura,
            protocol,
            model: nonempty(options.model)
                .or(default_sakura_model)
                .unwrap_or_else(|| default_engine_model(tts, sakura)),
            voice: nonempty(options.voice)
                .or(default_sakura_voice)
                .unwrap_or_else(|| if sakura { "normal" } else { "alloy" }.into()),
            language: nonempty(options.language)
                .or_else(|| hints.first().cloned())
                .or_else(|| (sakura && !tts).then(|| "ja".into())),
            response_format: nonempty(options.response_format)
                .unwrap_or_else(|| if sakura { "wav" } else { "pcm" }.into()),
            path: endpoint_path(
                options.path,
                if tts {
                    "/audio/speech"
                } else {
                    "/audio/transcriptions"
                },
            ),
            query_path: endpoint_path(
                options.audio_query_path,
                if use_sakura_tts_v1 {
                    "/tts/v1/audio_query"
                } else {
                    "/audio_query"
                },
            ),
            synthesis_path: endpoint_path(
                options.synthesis_path,
                if use_sakura_tts_v1 {
                    "/tts/v1/synthesis"
                } else {
                    "/synthesis"
                },
            ),
            speaker: options
                .speaker
                .map_or(default_speaker, |s| s.min(u64::from(u32::MAX)) as u32),
            speed: options.speed.map(|speed| speed as f32),
            pitch: None,
            style: None,
            timeout,
            secret,
            client,
        })
    }

    pub fn with_binding(mut self, binding: &ene_domain::VoiceBinding) -> Self {
        if let Some(speed) = binding.speed
            && speed.is_finite()
        {
            self.speed = Some(speed.clamp(0.5, 2.0));
        }
        if let Some(pitch) = binding.pitch
            && pitch.is_finite()
        {
            self.pitch = Some(pitch.clamp(-0.15, 0.15));
        }
        if let Some(style) = &binding.style
            && !style.trim().is_empty()
        {
            self.style = Some(style.trim().to_owned());
        }
        self
    }

    async fn exchange(&self, call: reqwest::RequestBuilder, what: &str) -> HttpResult<Vec<u8>> {
        let response = post(call.timeout(self.timeout), self.secret.as_deref(), what).await?;
        read_response_body(response, MAX_BODY_BYTES, what).await
    }

    async fn openai_tts(&self, text: String, voice: String) -> HttpResult<(Vec<u8>, u32)> {
        let trimmed = voice.trim();
        let (req_model, req_voice) = if self.sakura {
            if trimmed.is_empty() {
                (self.model.as_str(), self.voice.as_str())
            } else if let Some((m, v)) = trimmed.split_once('/') {
                let m = m.trim();
                let v = v.trim();
                (
                    if m.is_empty() { self.model.as_str() } else { m },
                    if v.is_empty() { "normal" } else { v },
                )
            } else if is_sakura_tts_model_id(trimmed) {
                (trimmed, self.voice.as_str())
            } else {
                (self.model.as_str(), trimmed)
            }
        } else {
            (
                self.model.as_str(),
                if trimmed.is_empty() {
                    self.voice.as_str()
                } else {
                    trimmed
                },
            )
        };
        let mut body = serde_json::json!({
            "model": req_model,
            "voice": req_voice,
            "input": text,
            "response_format": self.response_format,
        });
        if !self.sakura
            && let Some(speed) = self.speed
        {
            body["speed"] = serde_json::json!(speed);
        }
        if !self.sakura
            && let Some(style) = &self.style
        {
            body["instructions"] = serde_json::json!(style);
        }
        let mut call = self
            .client
            .post(format!("{}{}", self.base, self.path))
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if self.response_format.eq_ignore_ascii_case("wav") {
            call = call.header(reqwest::header::ACCEPT, "audio/wav");
        }
        let bytes = self.exchange(call.body(body.to_string()), "TTS").await?;
        if (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE")
            || self.response_format.eq_ignore_ascii_case("wav")
        {
            return parse_wav(&bytes);
        }
        Ok((bytes, SAMPLE_RATE))
    }

    async fn voicevox(&self, text: String, voice: String) -> HttpResult<(Vec<u8>, u32)> {
        let speaker = voice
            .trim()
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .parse::<u32>()
            .ok()
            .unwrap_or(self.speaker);
        let encoded: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
        let call = self.client.post(format!(
            "{}{}?text={encoded}&speaker={speaker}",
            self.base, self.query_path
        ));
        let mut body: serde_json::Value =
            serde_json::from_slice(&self.exchange(call, "TTS").await?).map_err(|_| {
                VoiceHttpError::Decode("the TTS endpoint answered a non-JSON audio query".into())
            })?;
        if let Some(speed) = self.speed {
            body["speedScale"] = serde_json::json!(speed);
        }
        if let Some(pitch) = self.pitch {
            body["pitchScale"] = serde_json::json!(pitch);
        }
        let call = self
            .client
            .post(format!(
                "{}{}?speaker={speaker}",
                self.base, self.synthesis_path
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        parse_wav(&self.exchange(call, "TTS").await?)
    }

    async fn openai_stt(&self, pcm: Vec<u8>, sample_rate: u32) -> HttpResult<String> {
        let (pcm_16k, stt_rate) = if sample_rate != 16_000 {
            (resample_mono(&pcm, sample_rate, 16_000, &mut None)?, 16_000)
        } else {
            (pcm, sample_rate)
        };
        let file = reqwest::multipart::Part::bytes(wav_wrap(&pcm_16k, stt_rate)?)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|error| {
                VoiceHttpError::InvalidInput(format!("the STT multipart body: {error}"))
            })?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("model", self.model.clone());
        if let Some(language) = &self.language {
            form = form.text("language", language.clone());
        }
        if !self.sakura {
            form = form.text("response_format", "json");
        }
        let call = self
            .client
            .post(format!("{}{}", self.base, self.path))
            .multipart(form);
        let body = self.exchange(call, "STT").await?;
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
            VoiceHttpError::Decode("the STT endpoint answered a non-JSON transcript".into())
        })?;
        Ok(value["text"].as_str().unwrap_or_default().trim().to_owned())
    }

    async fn raw_stt(&self, pcm: Vec<u8>, sample_rate: u32) -> HttpResult<String> {
        let (pcm_16k, stt_rate) = if sample_rate != 16_000 {
            (resample_mono(&pcm, sample_rate, 16_000, &mut None)?, 16_000)
        } else {
            (pcm, sample_rate)
        };
        let call = self
            .client
            .post(format!("{}{}", self.base, self.path))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(wav_wrap(&pcm_16k, stt_rate)?);
        String::from_utf8(self.exchange(call, "STT").await?)
            .map(|text| text.trim().to_owned())
            .map_err(|_| {
                VoiceHttpError::Decode("the STT endpoint answered a non-UTF-8 transcript".into())
            })
    }
}

impl TtsEngine for HttpEngine {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            match self.protocol {
                Protocol::OpenAiTts => self.openai_tts(text, voice).await,
                Protocol::Voicevox => self.voicevox(text, voice).await,
                Protocol::OpenAiStt | Protocol::Raw => Err(VoiceHttpError::Protocol(
                    "the TTS engine received an STT protocol".into(),
                )),
            }
        })
    }
}

impl SttEngine for HttpEngine {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move {
            match self.protocol {
                Protocol::OpenAiStt => self.openai_stt(pcm, sample_rate).await,
                Protocol::Raw => self.raw_stt(pcm, sample_rate).await,
                Protocol::OpenAiTts | Protocol::Voicevox => Err(VoiceHttpError::Protocol(
                    "the STT engine received a TTS protocol".into(),
                )),
            }
        })
    }
}

fn parse_options<T: DeserializeOwned + Default>(
    endpoint: &Endpoint,
    label: &str,
) -> Result<T, SetupError> {
    if endpoint.options.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&endpoint.options).map_err(|error| {
        SetupError::Unsupported(format!(
            "the {label} connection options are not JSON: {error}"
        ))
    })
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
