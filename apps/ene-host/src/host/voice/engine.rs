//! Speech synthesis and recognition engine resolution (`HttpEngine` and plugin adapters).
use super::{
    BoxFuture, HttpResult,
    pcm::{SAMPLE_RATE, parse_wav, resample_mono, wav_wrap},
    plugin,
};
use crate::host::{
    db::Store,
    mind::Companion,
    model::{self, Credentials},
    net,
    providers::Providers,
    settings::{Connection, ConnectionConfig, ConnectionKind, Endpoint},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde::{Deserialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupError {
    Missing,
    Disabled(String),
    Broken(String),
    Unsupported(String),
}

pub(crate) trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>>;
}

pub(crate) trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>>;
}

/// Engines a test injects to stand in for the plugin path.
#[derive(Resource, Default)]
pub(crate) struct Engines {
    pub(crate) tts: Option<Arc<dyn TtsEngine>>,
    pub(crate) stt: Option<Arc<dyn SttEngine>>,
}

#[derive(Deserialize, Default)]
struct SpeechOptions {
    protocol: Option<String>,
    model: Option<String>,
    voice: Option<String>,
    language: Option<String>,
    response_format: Option<String>,
    path: Option<String>,
    audio_query_path: Option<String>,
    synthesis_path: Option<String>,
    speaker: Option<u64>,
    speed: Option<f64>,
    timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    OpenAiTts,
    Voicevox,
    OpenAiStt,
    Raw,
}

pub(super) struct HttpEngine {
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
}

impl HttpEngine {
    pub(super) fn new(
        connection: Connection,
        credentials: Option<&Credentials>,
        providers: Option<&Providers>,
        kind: ConnectionKind,
    ) -> std::result::Result<Self, SetupError> {
        let tts = kind == ConnectionKind::Tts;
        let label = if tts { "TTS" } else { "STT" };
        let endpoint = connection.endpoint().clone();
        let resolved_base = resolve_base(&connection, providers, label)?;
        let options: SpeechOptions = parse_options(&endpoint, label)?;
        let secret = match credentials {
            Some(creds) => model::connection_secret(
                &connection,
                Some(creds),
                providers.unwrap_or(&Providers::none()),
            )
            .map_err(|error| SetupError::Broken(error.to_string()))?,
            None => None,
        };
        let protocol = match (tts, nonempty(options.protocol).as_deref()) {
            (true, None) if endpoint.adapter == "voicevox" => Protocol::Voicevox,
            (true, None | Some("openai" | "sakura")) => Protocol::OpenAiTts,
            (true, Some("voicevox")) => Protocol::Voicevox,
            (false, None | Some("openai" | "sakura")) => Protocol::OpenAiStt,
            (false, Some("raw")) => Protocol::Raw,
            (_, Some(other)) => {
                return Err(SetupError::Unsupported(format!(
                    "the {label} adapter protocol `{other}` is not supported"
                )));
            }
        };
        let sakura = model::is_sakura_connection(&endpoint.adapter, &resolved_base);
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
        let (default_sakura_model, default_sakura_voice) = match default_voice.as_deref() {
            Some(raw) if sakura => {
                let trimmed = raw.trim();
                if let Some((m, v)) = trimmed.split_once('/') {
                    let m = m.trim();
                    let v = v.trim();
                    (
                        (!m.is_empty()).then(|| m.to_owned()),
                        (!v.is_empty()).then(|| v.to_owned()),
                    )
                } else if !trimmed.is_empty() && model::is_sakura_tts_model_id(trimmed) {
                    (Some(trimmed.to_owned()), None)
                } else {
                    (None, (!trimmed.is_empty()).then(|| trimmed.to_owned()))
                }
            }
            _ => (None, default_voice.clone()),
        };
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
                .unwrap_or_else(|| {
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
                }),
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
                .map(|s| s.min(u64::from(u32::MAX)) as u32)
                .unwrap_or(default_speaker),
            speed: options.speed.map(|speed| speed as f32),
            pitch: None,
            style: None,
            timeout,
            secret,
        })
    }

    pub(super) fn with_binding(mut self, binding: &w::VoiceBinding) -> Self {
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
        net::read_response_body(response, MAX_BODY_BYTES, what).await
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
            } else if model::is_sakura_tts_model_id(trimmed) {
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
        let mut call = net::client()?
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
        let client = net::client()?;
        let call = client.post(format!(
            "{}{}?text={encoded}&speaker={speaker}",
            self.base, self.query_path
        ));
        let mut body: serde_json::Value =
            serde_json::from_slice(&self.exchange(call, "TTS").await?)
                .map_err(|_| "the TTS endpoint answered a non-JSON audio query".to_owned())?;
        if let Some(speed) = self.speed {
            body["speedScale"] = serde_json::json!(speed);
        }
        if let Some(pitch) = self.pitch {
            body["pitchScale"] = serde_json::json!(pitch);
        }
        let call = client
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
            .map_err(|error| format!("the STT multipart body: {error}"))?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("model", self.model.clone());
        if let Some(language) = &self.language {
            form = form.text("language", language.clone());
        }
        if !self.sakura {
            form = form.text("response_format", "json");
        }
        let call = net::client()?
            .post(format!("{}{}", self.base, self.path))
            .multipart(form);
        let body = self.exchange(call, "STT").await?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| "the STT endpoint answered a non-JSON transcript".to_owned())?;
        Ok(value["text"].as_str().unwrap_or_default().trim().to_owned())
    }

    async fn raw_stt(&self, pcm: Vec<u8>, sample_rate: u32) -> HttpResult<String> {
        let (pcm_16k, stt_rate) = if sample_rate != 16_000 {
            (resample_mono(&pcm, sample_rate, 16_000, &mut None)?, 16_000)
        } else {
            (pcm, sample_rate)
        };
        let call = net::client()?
            .post(format!("{}{}", self.base, self.path))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(wav_wrap(&pcm_16k, stt_rate)?);
        String::from_utf8(self.exchange(call, "STT").await?)
            .map(|text| text.trim().to_owned())
            .map_err(|_| "the STT endpoint answered a non-UTF-8 transcript".to_owned())
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
                Protocol::OpenAiStt | Protocol::Raw => {
                    Err("the TTS engine received an STT protocol".into())
                }
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
                Protocol::OpenAiTts | Protocol::Voicevox => {
                    Err("the STT engine received a TTS protocol".into())
                }
            }
        })
    }
}

async fn post(
    call: reqwest::RequestBuilder,
    secret: Option<&str>,
    label: &str,
) -> HttpResult<reqwest::Response> {
    let call = match secret {
        Some(secret) => call.bearer_auth(secret),
        None => call,
    };
    let response = call
        .send()
        .await
        .map_err(|error| format!("the {label} endpoint is unreachable: {error}"))?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(format!(
            "the {label} endpoint answered {}",
            response.status().as_u16()
        ))
    }
}

fn resolve_base(
    connection: &Connection,
    providers: Option<&Providers>,
    label: &str,
) -> std::result::Result<String, SetupError> {
    let fallback = Providers::none();
    let providers = providers.unwrap_or(&fallback);
    let raw = crate::host::settings::endpoint_of(connection, providers).unwrap_or_else(|| {
        connection
            .endpoint()
            .endpoint
            .trim()
            .trim_end_matches('/')
            .to_owned()
    });
    let base = raw.trim().trim_end_matches('/');
    url::Url::parse(base)
        .map_err(|_| SetupError::Broken(format!("the {label} connection endpoint is not a URL")))?;
    Ok(base.to_owned())
}

fn parse_options<T: DeserializeOwned + Default>(
    endpoint: &Endpoint,
    label: &str,
) -> std::result::Result<T, SetupError> {
    if endpoint.options.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&endpoint.options).map_err(|error| {
        SetupError::Unsupported(format!(
            "the {label} connection options are not JSON: {error}"
        ))
    })
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn endpoint_path(value: Option<String>, default: &str) -> String {
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

pub(crate) fn parse_binding(companion: &Companion) -> w::VoiceBinding {
    let mut binding =
        serde_json::from_value::<w::VoiceBinding>(companion.voice.json.clone()).unwrap_or_default();
    if binding
        .voice_id
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        binding.voice_id = companion.voice_identifier.clone();
    }
    binding
}

pub(crate) fn tts_source(world: &World) -> std::result::Result<Arc<dyn TtsEngine>, SetupError> {
    companion_tts_source(world, None)
}

pub(crate) fn companion_tts_source(
    world: &World,
    companion: Option<&Companion>,
) -> std::result::Result<Arc<dyn TtsEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.tts.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let binding = companion.map(parse_binding).unwrap_or_default();
    let connection = match binding
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(connection_id) => voice_connection_by_id(world, ConnectionKind::Tts, connection_id)?,
        None => voice_connection(world, ConnectionKind::Tts)?,
    };
    if let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Tts)? {
        return Ok(Arc::new(engine));
    }
    if matches!(
        connection.endpoint().adapter.as_str(),
        "voicevox" | "openai" | "sakura"
    ) {
        let engine = HttpEngine::new(
            connection,
            world.get_resource::<Credentials>(),
            world.get_resource::<Providers>(),
            ConnectionKind::Tts,
        )?
        .with_binding(&binding);
        return Ok(Arc::new(engine));
    }
    Err(SetupError::Unsupported(format!(
        "the TTS adapter `{}` is not served by an installed plugin",
        connection.endpoint().adapter
    )))
}

pub(crate) fn stt_source(world: &World) -> std::result::Result<Arc<dyn SttEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.stt.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let connection = voice_connection(world, ConnectionKind::Stt)?;
    if let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Stt)? {
        return Ok(Arc::new(engine));
    }
    if matches!(connection.endpoint().adapter.as_str(), "openai" | "sakura") {
        let engine = HttpEngine::new(
            connection,
            world.get_resource::<Credentials>(),
            world.get_resource::<Providers>(),
            ConnectionKind::Stt,
        )?;
        return Ok(Arc::new(engine));
    }
    Err(SetupError::Unsupported(format!(
        "the STT adapter `{}` is not served by an installed plugin",
        connection.endpoint().adapter
    )))
}

/// The plugin engine for `connection`, when a plugin serves its adapter.
fn plugin_engine(
    world: &World,
    connection: &Connection,
    kind: ConnectionKind,
) -> std::result::Result<Option<plugin::PluginSpeech>, SetupError> {
    let Some(providers) = world.get_resource::<Providers>() else {
        return Ok(None);
    };
    plugin::PluginSpeech::new(
        providers,
        connection,
        world.get_resource::<Credentials>(),
        kind,
    )
}

fn voice_connection_by_id(
    world: &World,
    kind: ConnectionKind,
    id: &str,
) -> std::result::Result<Connection, SetupError> {
    let Some(store) = world.get_resource::<Store>() else {
        return Err(SetupError::Broken("the host state is not available".into()));
    };
    let settings = store
        .settings()
        .map_err(|error| SetupError::Broken(error.to_string()))?;
    let Some(connection) = settings
        .connections
        .into_iter()
        .find(|connection| connection.id == id && connection.kind == kind)
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
    Ok(connection)
}

pub(super) fn voice_connection(
    world: &World,
    kind: ConnectionKind,
) -> std::result::Result<Connection, SetupError> {
    let Some(store) = world.get_resource::<Store>() else {
        return Err(SetupError::Broken("the host state is not available".into()));
    };
    let settings = store
        .settings()
        .map_err(|error| SetupError::Broken(error.to_string()))?;
    let mut first_disabled = None;
    for connection in settings.connections {
        if connection.kind != kind {
            continue;
        }
        if connection.enabled {
            return Ok(connection);
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
