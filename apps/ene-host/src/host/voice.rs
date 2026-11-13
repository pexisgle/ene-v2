//! The host-side voice pipeline: speech recognition input, sentence-level
//! speech synthesis output, playback ownership and the speech floor
//! (docs/design/voice-avatar.md §4–§5, docs/api.md §6–§7).
//!
//! The host thread only keeps state: it splits the streamed text into
//! sentences, assigns generations and `sequence_no`s, records the audio-owning
//! client and the floor, and enqueues synthesis jobs. A dedicated worker owns a
//! current-thread Tokio runtime, runs at most three syntheses per utterance and
//! releases the audio in `sequence_no` order. Recognition runs in the Connect
//! layer. Without a TTS/STT connection the capability is `unimplemented`.
use super::{
    agent,
    db::{self, Result, Store},
    error::HostError,
    model::Credentials,
    notify::Notifier,
    settings::{Connection, ConnectionConfig, ConnectionKind, Endpoint},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::{Aside, Author, RequestId};
use rig_ecs::prelude::*;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// The wire standard: 24 kHz, 16-bit signed little-endian, one channel.
pub(crate) const SAMPLE_RATE: u32 = 24_000;
/// The sample rates a client may send for recognition (host.proto §voice).
pub(crate) const STT_SAMPLE_RATES: [u32; 2] = [24_000, 16_000];
/// The design caps the synthesis queue at three sentences per utterance.
const MAX_SYNTHESIS_SENTENCES: usize = 3;
/// One sentence's synthesis, then it is reported skipped.
pub(crate) const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(20);
/// One recognition request's deadline.
pub(crate) const STT_TIMEOUT: Duration = Duration::from_secs(30);
/// Client chunks are folded into sentence-sized audio envelopes.
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
/// A sentence never grows past this: unpunctuated text still gets spoken.
const MAX_SENTENCE_CHARS: usize = 240;
/// A pause this long ends the recognition utterance.
pub(crate) const STT_SILENCE_WINDOW: Duration = Duration::from_millis(700);
/// Shorter bursts are noise, not speech.
pub(crate) const MIN_STT_BYTES: usize = 3_200;
/// The longest audio one recognition request carries (20 s at 24 kHz).
pub(crate) const MAX_STT_BYTES: usize = 48_000 * 20;
/// Release the floor this long after the expected playback end.
const PLAYBACK_GRACE: Duration = Duration::from_secs(3);
/// How many transcription ids are remembered to drop a duplicate delivery.
const TRANSCRIPTION_MEMORY: usize = 128;

/// The exact message the RPC edge returns when no engine is configured.
pub(crate) const VOICE_UNAVAILABLE: &str = "voice requires a configured TTS/STT connection";

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type HttpResult<T> = std::result::Result<T, String>;

/// Why a configured voice capability is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupError {
    /// No connection of that kind exists: the capability is absent.
    Missing,
    /// A connection exists but is switched off.
    Disabled(String),
    /// A connection exists but cannot do its job right now.
    Broken(String),
    /// The connection names an adapter or protocol the host does not speak.
    Unsupported(String),
}

/// The speech-synthesis engine seam. `synthesize` returns mono 16-bit PCM and
/// its sample rate; the pipeline converts it to the wire standard.
pub(crate) trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>>;
}

/// The speech-recognition engine seam. `transcribe` returns the transcript.
pub(crate) trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtsProtocol {
    /// `POST {base}{path}` `/v1/audio/speech`, `response_format: pcm`.
    OpenAi,
    /// VOICEVOX: `audio_query` then `synthesis`.
    Voicevox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SttProtocol {
    /// OpenAI-compatible multipart `/v1/audio/transcriptions`.
    OpenAi,
    /// A plain body of WAV bytes whose response is the transcript text.
    Raw,
}

#[derive(Clone)]
pub(crate) struct TtsSource {
    key: Option<String>,
    engine: Arc<dyn TtsEngine>,
}

/// The HTTP text-to-speech adapter. The OpenAI protocol asks for raw `pcm`
/// (24 kHz, 16-bit, mono, little-endian); the VOICEVOX protocol takes a WAV
/// back and converts it to the wire standard.
struct HttpTtsEngine {
    client: HttpResult<reqwest::Client>,
    base: String,
    protocol: TtsProtocol,
    model: String,
    voice: String,
    path: String,
    query_path: String,
    synthesis_path: String,
    speaker: u32,
    speed: Option<f32>,
    secret: Option<String>,
}

impl HttpTtsEngine {
    fn new(
        connection: Connection,
        credentials: Option<&Credentials>,
    ) -> std::result::Result<Self, SetupError> {
        let endpoint = connection.endpoint().clone();
        let base = base_url(&endpoint, "TTS")?;
        let secret = credential(&connection, credentials, &base, "TTS")?;
        let options = option_map(&endpoint)?;
        let protocol_name = option_str(&options, "protocol").unwrap_or_else(|| {
            if endpoint.adapter == "voicevox" {
                "voicevox"
            } else {
                "openai"
            }
        });
        let protocol = match protocol_name {
            "openai" => TtsProtocol::OpenAi,
            "voicevox" => TtsProtocol::Voicevox,
            other => {
                return Err(SetupError::Unsupported(format!(
                    "the TTS adapter protocol `{other}` is not supported"
                )));
            }
        };
        let default_voice = match &connection.config {
            ConnectionConfig::Tts(config) => config.voice_ids.first().cloned(),
            _ => None,
        };
        let client = http_client(option_u64(&options, "timeout_ms"), 20_000);
        Ok(Self {
            client,
            base,
            protocol,
            model: option_str(&options, "model")
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "tts-1".into()),
            voice: option_str(&options, "voice")
                .map(ToOwned::to_owned)
                .or(default_voice)
                .unwrap_or_else(|| "alloy".into()),
            path: path_option(&options, "path", "/audio/speech"),
            query_path: path_option(&options, "audio_query_path", "/audio_query"),
            synthesis_path: path_option(&options, "synthesis_path", "/synthesis"),
            speaker: option_u64(&options, "speaker").unwrap_or(1) as u32,
            speed: option_f64(&options, "speed").map(|speed| speed as f32),
            secret,
        })
    }

    fn client(&self) -> HttpResult<&reqwest::Client> {
        self.client.as_ref().map_err(Clone::clone)
    }

    async fn openai(&self, text: String, voice: String) -> HttpResult<(Vec<u8>, u32)> {
        let mut body = serde_json::json!({
            "model": self.model,
            "voice": if voice.is_empty() { &self.voice } else { &voice },
            "input": text,
            "response_format": "pcm",
        });
        if let Some(speed) = self.speed {
            body["speed"] = serde_json::json!(speed);
        }
        let call = self
            .client()?
            .post(format!("{}{}", self.base, self.path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let response = post(call, self.secret.as_deref(), "TTS").await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("the TTS endpoint body: {error}"))?;
        Ok((bytes.to_vec(), SAMPLE_RATE))
    }

    async fn voicevox(&self, text: String) -> HttpResult<(Vec<u8>, u32)> {
        let encoded: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
        let query_url = format!(
            "{}{}?text={}&speaker={}",
            self.base, self.query_path, encoded, self.speaker
        );
        let call = self.client()?.post(query_url);
        let query = post(call, self.secret.as_deref(), "TTS").await?;
        let text = query
            .text()
            .await
            .map_err(|error| format!("the TTS endpoint body: {error}"))?;
        let mut body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| "the TTS endpoint answered a non-JSON audio query".to_owned())?;
        if let Some(speed) = self.speed {
            body["speedScale"] = serde_json::json!(speed);
        }
        let synthesis_url = format!(
            "{}{}?speaker={}",
            self.base, self.synthesis_path, self.speaker
        );
        let call = self
            .client()?
            .post(synthesis_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let response = post(call, self.secret.as_deref(), "TTS").await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("the TTS endpoint body: {error}"))?;
        parse_wav(&bytes)
    }
}

impl TtsEngine for HttpTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            match self.protocol {
                TtsProtocol::OpenAi => self.openai(text, voice).await,
                TtsProtocol::Voicevox => self.voicevox(text).await,
            }
        })
    }
}

/// The HTTP speech-recognition adapter.
struct HttpSttEngine {
    client: HttpResult<reqwest::Client>,
    base: String,
    protocol: SttProtocol,
    path: String,
    model: String,
    language: Option<String>,
    secret: Option<String>,
}

impl HttpSttEngine {
    fn new(
        connection: Connection,
        credentials: Option<&Credentials>,
    ) -> std::result::Result<Self, SetupError> {
        let endpoint = connection.endpoint().clone();
        let base = base_url(&endpoint, "STT")?;
        let secret = credential(&connection, credentials, &base, "STT")?;
        let options = option_map(&endpoint)?;
        let protocol = match option_str(&options, "protocol").unwrap_or("openai") {
            "openai" => SttProtocol::OpenAi,
            "raw" => SttProtocol::Raw,
            other => {
                return Err(SetupError::Unsupported(format!(
                    "the STT adapter protocol `{other}` is not supported"
                )));
            }
        };
        let hints: &[String] = match &connection.config {
            ConnectionConfig::Stt(config) => &config.language_hints,
            _ => &[],
        };
        let client = http_client(option_u64(&options, "timeout_ms"), 30_000);
        Ok(Self {
            client,
            base,
            protocol,
            path: path_option(&options, "path", "/audio/transcriptions"),
            model: option_str(&options, "model")
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "whisper-1".into()),
            language: option_str(&options, "language")
                .map(ToOwned::to_owned)
                .or_else(|| hints.first().cloned()),
            secret,
        })
    }
}

impl SttEngine for HttpSttEngine {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move {
            let client = self.client.as_ref().map_err(Clone::clone)?;
            let url = format!("{}{}", self.base, self.path);
            let wav = wav_wrap(&pcm, sample_rate);
            let response = match self.protocol {
                SttProtocol::OpenAi => {
                    let file = reqwest::multipart::Part::bytes(wav)
                        .file_name("audio.wav")
                        .mime_str("audio/wav")
                        .map_err(|error| format!("the STT multipart body: {error}"))?;
                    let mut form = reqwest::multipart::Form::new()
                        .part("file", file)
                        .text("model", self.model.clone());
                    if let Some(language) = &self.language {
                        form = form.text("language", language.clone());
                    }
                    post(
                        client
                            .post(url)
                            .multipart(form.text("response_format", "json")),
                        self.secret.as_deref(),
                        "STT",
                    )
                    .await?
                }
                SttProtocol::Raw => {
                    post(
                        client
                            .post(url)
                            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                            .body(wav),
                        self.secret.as_deref(),
                        "STT",
                    )
                    .await?
                }
            };
            let text = response
                .text()
                .await
                .map_err(|error| format!("the STT endpoint body: {error}"))?;
            match self.protocol {
                SttProtocol::OpenAi => {
                    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                        "the STT endpoint answered a non-JSON transcript".to_owned()
                    })?;
                    Ok(value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned())
                }
                SttProtocol::Raw => Ok(text.trim().to_owned()),
            }
        })
    }
}

/// Posts a request with the connection's secret and maps transport and status
/// failures onto the sentence-level error text.
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

fn http_client(timeout_ms: Option<u64>, default_timeout_ms: u64) -> HttpResult<reqwest::Client> {
    let timeout = Duration::from_millis(
        timeout_ms
            .unwrap_or(default_timeout_ms)
            .clamp(1_000, 300_000),
    );
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| format!("http client: {error}"))
}

/// Resolves the TTS engine, or why the capability is unavailable.
pub(crate) fn tts_source(world: &World) -> std::result::Result<TtsSource, SetupError> {
    #[cfg(test)]
    if let Some(engine) = world
        .get_resource::<TestEngines>()
        .and_then(|test| test.tts.as_ref())
    {
        return Ok(TtsSource {
            key: None,
            engine: Arc::clone(engine),
        });
    }
    let connection = voice_connection(world, ConnectionKind::Tts)?;
    let key = format!("{}:{}", connection.id, connection.revision);
    let engine = HttpTtsEngine::new(connection, world.get_resource::<Credentials>())?;
    Ok(TtsSource {
        key: Some(key),
        engine: Arc::new(engine),
    })
}

/// Resolves the STT engine, or why the capability is unavailable.
pub(crate) fn stt_source(world: &World) -> std::result::Result<Arc<dyn SttEngine>, SetupError> {
    #[cfg(test)]
    if let Some(engine) = world
        .get_resource::<TestEngines>()
        .and_then(|test| test.stt.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let connection = voice_connection(world, ConnectionKind::Stt)?;
    let engine = HttpSttEngine::new(connection, world.get_resource::<Credentials>())?;
    Ok(Arc::new(engine))
}

pub(crate) fn configured(
    world: &World,
    kind: ConnectionKind,
) -> std::result::Result<(), SetupError> {
    voice_connection(world, kind).map(|_| ())
}

fn voice_connection(
    world: &World,
    kind: ConnectionKind,
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
        .find(|connection| connection.kind == kind)
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

fn base_url(endpoint: &Endpoint, label: &str) -> std::result::Result<String, SetupError> {
    let base = endpoint.endpoint.trim().trim_end_matches('/').to_owned();
    if base.is_empty() {
        return Err(SetupError::Broken(format!(
            "the {label} connection has no endpoint"
        )));
    }
    let parsed = url::Url::parse(&base)
        .map_err(|_| SetupError::Broken(format!("the {label} connection endpoint is not a URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SetupError::Broken(format!(
            "the {label} connection endpoint must be http(s)"
        )));
    }
    Ok(base)
}

fn credential(
    connection: &Connection,
    credentials: Option<&Credentials>,
    base: &str,
    label: &str,
) -> std::result::Result<Option<String>, SetupError> {
    let secret = credentials.and_then(|credentials| secret_for(connection, credentials));
    if secret.is_none() && !loopback_endpoint(base) {
        return Err(SetupError::Broken(format!(
            "no credential is registered for the {label} connection"
        )));
    }
    Ok(secret)
}

fn secret_for(connection: &Connection, credentials: &Credentials) -> Option<String> {
    if let Some(secret) = credentials.secret(&connection.id) {
        return Some(secret);
    }
    if let Some(name) = connection.environment_variable()
        && let Ok(secret) = std::env::var(name)
        && !secret.is_empty()
    {
        return Some(secret);
    }
    // A loopback endpoint is a local server; it may accept any key.
    loopback_endpoint(&connection.endpoint().endpoint).then(|| "ene-local".into())
}

fn loopback_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn option_map(
    endpoint: &Endpoint,
) -> std::result::Result<serde_json::Map<String, serde_json::Value>, SetupError> {
    if endpoint.options.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str(&endpoint.options) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(SetupError::Unsupported(
            "the connection options are not a JSON object".into(),
        )),
        Err(error) => Err(SetupError::Unsupported(format!(
            "the connection options are not JSON: {error}"
        ))),
    }
}

fn option_str<'a>(
    options: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    options.get(key)?.as_str().filter(|value| !value.is_empty())
}

fn option_u64(options: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
    options.get(key)?.as_u64()
}

fn option_f64(options: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<f64> {
    options.get(key)?.as_f64()
}

fn path_option(
    options: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: &str,
) -> String {
    let path = option_str(options, key).unwrap_or(default);
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Converts one engine's audio to the wire standard.
fn wire_pcm(pcm: Vec<u8>, sample_rate: u32) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err("the TTS engine returned an odd number of PCM bytes".into());
    }
    if sample_rate == 0 {
        return Err("the TTS engine returned audio without a sample rate".into());
    }
    if sample_rate == SAMPLE_RATE {
        return Ok(pcm);
    }
    Ok(resample_mono(&pcm, sample_rate, SAMPLE_RATE))
}

/// Linear interpolation from one mono s16le rate to another.
fn resample_mono(pcm: &[u8], from: u32, to: u32) -> Vec<u8> {
    if from == to || from == 0 {
        return pcm.to_vec();
    }
    let samples: Vec<i16> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    let ratio = f64::from(to) / f64::from(from);
    let length = ((samples.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(length * 2);
    for index in 0..length {
        let position = index as f64 / ratio;
        let left = position.floor() as usize;
        let first = f64::from(samples.get(left).copied().unwrap_or(0));
        let second = f64::from(samples.get(left + 1).copied().unwrap_or(first as i16));
        let value = first + (second - first) * (position - left as f64);
        let sample = value
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// A minimal 16-bit mono PCM WAV container for the STT endpoint.
fn wav_wrap(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads a mono 16-bit WAV into PCM plus its sample rate. VOICEVOX answers a
/// WAV while the wire standard is raw PCM, so the container must be unpacked.
fn parse_wav(bytes: &[u8]) -> HttpResult<(Vec<u8>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("the TTS endpoint did not return a WAV file".into());
    }
    let (mut format, mut data) = (None, None);
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let size = le32(bytes, offset + 4) as usize;
        let start = offset + 8;
        let end = start.saturating_add(size).min(bytes.len());
        match &bytes[offset..offset + 4] {
            b"fmt " if size >= 16 => {
                format = Some((
                    le16(bytes, start + 2),
                    le32(bytes, start + 4),
                    le16(bytes, start + 14),
                ));
            }
            b"data" => data = Some(&bytes[start..end]),
            _ => {}
        }
        offset = start + size + (size & 1);
    }
    let (channels, rate, bits) =
        format.ok_or_else(|| "the TTS endpoint returned a WAV without a format".to_owned())?;
    let data = data.ok_or_else(|| "the TTS endpoint returned a WAV without audio".to_owned())?;
    if bits != 16 {
        return Err("the TTS endpoint returned audio that is not 16-bit PCM".into());
    }
    let channels = channels.max(1) as usize;
    if channels == 1 {
        return Ok((data.to_vec(), rate));
    }
    let mut mono = Vec::with_capacity(data.len() / channels);
    for frame in data.chunks_exact(2 * channels) {
        let sum: i32 = (0..channels)
            .map(|channel| i32::from(le16(frame, channel * 2)))
            .sum();
        let sample = (sum / channels as i32).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
        mono.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    Ok((mono, rate))
}

#[derive(Clone)]
struct Stream {
    subscription: String,
    /// Audio and the end-of-utterance markers; delivery waits for room.
    audio: mpsc::Sender<w::AudioOutputEvent>,
    /// Flushes and other control markers; never dropped for lack of room.
    control: mpsc::UnboundedSender<w::AudioOutputEvent>,
}

/// The client that owns one companion's audio output and the utterance it is
/// playing. The client is kept across utterances; the utterance is only set
/// while one is being sent.
#[derive(Clone, Default)]
struct Owner {
    client_id: String,
    activation_revision: u64,
    utterance_id: Option<String>,
    generation_id: u64,
    total_sentences: Option<u32>,
    deadline: Option<Instant>,
}

impl Owner {
    fn take_utterance(&mut self) -> Option<(String, u64)> {
        let utterance_id = self.utterance_id.take()?;
        self.total_sentences = None;
        self.deadline = None;
        Some((utterance_id, self.generation_id))
    }
}

#[derive(Clone)]
struct Utterance {
    id: String,
    generation_id: u64,
    next_sequence: u32,
    chars: u32,
    finished: bool,
    voice: String,
    source: TtsSource,
}

#[derive(Default)]
struct State {
    next_generation: u64,
    floor: Option<String>,
    streams: HashMap<String, Stream>,
    owners: HashMap<String, Owner>,
    utterances: HashMap<String, Utterance>,
    buffers: HashMap<String, String>,
    transcriptions: VecDeque<String>,
}

/// The live voice state, shared by the host thread and the synthesis worker.
/// Events for a generation the host already invalidated are dropped here.
#[derive(Clone, Resource)]
pub(crate) struct Voice {
    inner: Arc<Mutex<State>>,
    jobs: mpsc::UnboundedSender<Job>,
}

impl Voice {
    fn new() -> (Self, mpsc::UnboundedReceiver<Job>) {
        let (jobs, receiver) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(Mutex::new(State::default())),
                jobs,
            },
            receiver,
        )
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.inner)
    }

    fn shared(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.inner)
    }

    fn job(&self, job: Job) {
        if self.jobs.send(job).is_err() {
            tracing::debug!("the voice worker is gone; the sentence is dropped");
        }
    }

    fn register(&self, client: &str) -> AudioSubscription {
        let (audio, receiver) = mpsc::channel(64);
        let (control, control_receiver) = mpsc::unbounded_channel();
        let subscription_id = uuid::Uuid::new_v4().to_string();
        self.lock().streams.insert(
            client.to_owned(),
            Stream {
                subscription: subscription_id.clone(),
                audio,
                control,
            },
        );
        AudioSubscription {
            subscription_id,
            receiver,
            control: control_receiver,
        }
    }

    fn unregister(&self, client: &str, subscription: &str) {
        let mut state = self.lock();
        if state
            .streams
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            state.streams.remove(client);
        }
    }

    fn publish(&self, companion: &str, client: &str, revision: u64) {
        let mut state = self.lock();
        let owner = state
            .owners
            .entry(companion.to_owned())
            .or_insert_with(|| Owner {
                client_id: client.to_owned(),
                activation_revision: revision,
                ..Default::default()
            });
        owner.client_id = client.to_owned();
        owner.activation_revision = revision;
    }

    fn owner_of(&self, utterance_id: &str) -> Option<(String, Owner)> {
        let state = self.lock();
        state
            .owners
            .iter()
            .find(|(_, owner)| owner.utterance_id.as_deref() == Some(utterance_id))
            .map(|(companion, owner)| (companion.clone(), owner.clone()))
    }

    fn owns(&self, companion: &str) -> bool {
        self.lock().owners.contains_key(companion)
    }

    fn streaming(&self, companion: &str) -> Option<Utterance> {
        self.lock()
            .utterances
            .get(companion)
            .filter(|utterance| !utterance.finished)
            .cloned()
    }

    fn floor_holder(&self) -> Option<String> {
        self.lock().floor.clone()
    }

    /// Starts a new generation for `companion`: the previous utterance, if
    /// any, is flushed as cancelled and its queued synthesis invalidated.
    fn begin(&self, companion: &str, source: TtsSource, voice: String) -> bool {
        let mut state = self.lock();
        let Some(owner) = state.owners.get(companion).cloned() else {
            return false;
        };
        if let Some((utterance_id, generation_id)) = state
            .owners
            .get_mut(companion)
            .and_then(Owner::take_utterance)
        {
            if let Some(sender) = state
                .streams
                .get(&owner.client_id)
                .map(|stream| stream.control.clone())
            {
                let _ = sender.send(flush_event(
                    &utterance_id,
                    generation_id,
                    w::FlushReason::Cancelled,
                ));
            }
            self.job(Job::Invalidate { utterance_id });
        }
        state.next_generation += 1;
        let generation_id = state.next_generation;
        let id = uuid::Uuid::new_v4().to_string();
        let utterance = Utterance {
            id: id.clone(),
            generation_id,
            next_sequence: 0,
            chars: 0,
            finished: false,
            voice,
            source,
        };
        state.utterances.insert(companion.to_owned(), utterance);
        state.buffers.remove(companion);
        if let Some(owner) = state.owners.get_mut(companion) {
            owner.utterance_id = Some(id);
            owner.generation_id = generation_id;
            owner.total_sentences = None;
            owner.deadline = None;
        }
        if state
            .floor
            .as_deref()
            .is_none_or(|holder| holder == companion)
        {
            state.floor = Some(companion.to_owned());
        }
        true
    }

    /// Appends text and enqueues every complete sentence of it.
    fn speak(&self, companion: &str, text: &str) {
        let mut state = self.lock();
        let sentences = {
            let buffer = state.buffers.entry(companion.to_owned()).or_default();
            buffer.push_str(text);
            split_sentences(buffer)
        };
        if sentences.is_empty() {
            return;
        }
        let Some(utterance) = state.utterances.get_mut(companion) else {
            return;
        };
        let mut jobs = Vec::with_capacity(sentences.len());
        for text in sentences {
            let sequence_no = utterance.next_sequence;
            utterance.next_sequence += 1;
            utterance.chars += text.chars().count() as u32;
            jobs.push(Job::Sentence(sentence(
                companion,
                utterance,
                sequence_no,
                text,
            )));
        }
        drop(state);
        for job in jobs {
            self.job(job);
        }
    }

    /// Flushes the unterminated remainder and closes the utterance's send
    /// side. A turn that produced no text must not disturb playback.
    fn end_turn(&self, companion: &str) {
        let mut state = self.lock();
        if state.floor.as_deref() != Some(companion) {
            return;
        }
        let Some(utterance) = state.utterances.get(companion).cloned() else {
            self.retire(&mut state, companion);
            return;
        };
        if utterance.finished {
            return;
        }
        let remainder = state.buffers.get_mut(companion).and_then(flush_sentences);
        let Some(active) = state.utterances.get_mut(companion) else {
            return;
        };
        let mut jobs = Vec::new();
        if let Some(text) = remainder {
            let sequence_no = active.next_sequence;
            active.next_sequence += 1;
            active.chars += text.chars().count() as u32;
            jobs.push(Job::Sentence(sentence(
                companion,
                active,
                sequence_no,
                text,
            )));
        }
        active.finished = true;
        let total_sentences = active.next_sequence;
        let chars = active.chars;
        let utterance_id = active.id.clone();
        if total_sentences == 0 {
            self.retire(&mut state, companion);
            return;
        }
        for job in jobs {
            self.job(job);
        }
        self.job(Job::Finish {
            utterance_id,
            total_sentences,
        });
        if let Some(owner) = state.owners.get_mut(companion) {
            owner.total_sentences = Some(total_sentences);
            let speech = Duration::from_secs_f64((f64::from(chars) * 0.18).min(600.0));
            owner.deadline = Some(Instant::now() + speech + PLAYBACK_GRACE);
        }
    }

    /// Flushes playback: the client is told up to which generation to discard,
    /// queued and in-flight synthesis is invalidated and the floor freed.
    fn flush(&self, companion: &str, reason: w::FlushReason) -> Result<()> {
        let mut state = self.lock();
        let Some(owner) = state.owners.get(companion).cloned() else {
            return Err(HostError::not_found(
                "no client owns this companion's audio output",
            ));
        };
        if let Some((utterance_id, generation_id)) = state
            .owners
            .get_mut(companion)
            .and_then(Owner::take_utterance)
        {
            if let Some(sender) = state
                .streams
                .get(&owner.client_id)
                .map(|stream| stream.control.clone())
            {
                let _ = sender.send(flush_event(&utterance_id, generation_id, reason));
            }
            self.job(Job::Invalidate { utterance_id });
        }
        self.retire(&mut state, companion);
        Ok(())
    }

    fn client_moved(&self, companion: &str, client: &str, revision: u64) {
        let owns = self.lock().owners.get(companion).is_some_and(|owner| {
            owner.client_id == client && owner.activation_revision == revision
        });
        if !owns {
            return;
        }
        let _ = self.flush(companion, w::FlushReason::ClientMoved);
        self.lock().owners.remove(companion);
    }

    fn release(&self, companion: &str) {
        let mut state = self.lock();
        self.retire(&mut state, companion);
    }

    fn retire(&self, state: &mut State, companion: &str) {
        if let Some(utterance_id) = state
            .owners
            .get_mut(companion)
            .and_then(|owner| owner.take_utterance().map(|(id, _)| id))
        {
            self.job(Job::Invalidate { utterance_id });
        }
        state.utterances.remove(companion);
        state.buffers.remove(companion);
        if state.floor.as_deref() == Some(companion) {
            state.floor = None;
        }
        if let Some(owner) = state.owners.get_mut(companion) {
            owner.generation_id = 0;
        }
    }

    /// `false` when this transcription was already delivered.
    fn accept_transcription(&self, transcription_id: &str) -> bool {
        if transcription_id.is_empty() {
            return true;
        }
        let mut state = self.lock();
        if state
            .transcriptions
            .iter()
            .any(|known| known == transcription_id)
        {
            return false;
        }
        state.transcriptions.push_back(transcription_id.to_owned());
        if state.transcriptions.len() > TRANSCRIPTION_MEMORY {
            state.transcriptions.pop_front();
        }
        true
    }
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Pulls every complete sentence out of `buffer`, leaving the unterminated
/// remainder. A very long unpunctuated run is cut so speech still starts.
fn split_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    for (index, character) in buffer.char_indices() {
        if !matches!(character, '。' | '！' | '？' | '\n') {
            continue;
        }
        let end = index + character.len_utf8();
        let sentence = buffer[start..end].trim();
        if !sentence.is_empty() {
            sentences.push(sentence.to_owned());
        }
        start = end;
    }
    if start > 0 {
        buffer.drain(..start);
    }
    while buffer.chars().count() > MAX_SENTENCE_CHARS {
        let cut = buffer
            .char_indices()
            .nth(MAX_SENTENCE_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(buffer.len());
        let sentence = buffer[..cut].trim().to_owned();
        buffer.drain(..cut);
        if !sentence.is_empty() {
            sentences.push(sentence);
        }
    }
    sentences
}

/// The unterminated remainder as the final sentence of the turn.
fn flush_sentences(buffer: &mut String) -> Option<String> {
    let text = buffer.trim().to_owned();
    buffer.clear();
    (!text.is_empty()).then_some(text)
}

enum Job {
    Sentence(Sentence),
    /// No more sentences will arrive for this utterance.
    Finish {
        utterance_id: String,
        total_sentences: u32,
    },
    /// The host invalidated this utterance; forget its queued synthesis.
    Invalidate {
        utterance_id: String,
    },
}

struct Sentence {
    companion: String,
    utterance_id: String,
    generation_id: u64,
    sequence_no: u32,
    text: String,
    source: TtsSource,
    voice: String,
}

fn sentence(companion: &str, utterance: &Utterance, sequence_no: u32, text: String) -> Sentence {
    Sentence {
        companion: companion.to_owned(),
        utterance_id: utterance.id.clone(),
        generation_id: utterance.generation_id,
        sequence_no,
        text,
        source: utterance.source.clone(),
        voice: utterance.voice.clone(),
    }
}

/// Releases synthesized audio in `sequence_no` order. A sentence that finishes
/// first waits for the earlier ones; a failed sentence is announced at once so
/// the client never waits for it. A chunk is final when it closes its sentence.
struct Emitter {
    utterance_id: String,
    generation_id: u64,
    next: u32,
    finished: Option<u32>,
    sent_finished: bool,
    slots: Vec<Option<Slot>>,
}

enum Slot {
    Audio(Vec<u8>),
    Skipped,
}

impl Emitter {
    fn new(utterance_id: String, generation_id: u64) -> Self {
        Self {
            utterance_id,
            generation_id,
            next: 0,
            finished: None,
            sent_finished: false,
            slots: Vec::new(),
        }
    }

    /// Records one sentence's outcome and returns what to send now.
    fn offer(
        &mut self,
        sequence_no: u32,
        outcome: HttpResult<Vec<u8>>,
    ) -> Vec<w::AudioOutputEvent> {
        let mut emitted = Vec::new();
        if self.sent_finished {
            return emitted;
        }
        let index = sequence_no as usize;
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        match outcome {
            Ok(pcm) => self.slots[index] = Some(Slot::Audio(pcm)),
            Err(reason) => {
                // The client unblocks the moment it learns the number is gone.
                emitted.push(audio_event(w::SentenceSkipped {
                    utterance_id: self.utterance_id.clone(),
                    generation_id: self.generation_id,
                    sequence_no,
                    reason,
                    ..Default::default()
                }));
                self.slots[index] = Some(Slot::Skipped);
            }
        }
        self.drain(&mut emitted);
        self.emit_finished(&mut emitted);
        emitted
    }

    /// No more sentences will arrive for this utterance.
    fn finish(&mut self, total_sentences: u32) -> Vec<w::AudioOutputEvent> {
        self.finished = Some(total_sentences);
        let mut emitted = Vec::new();
        self.drain(&mut emitted);
        self.emit_finished(&mut emitted);
        emitted
    }

    fn drain(&mut self, emitted: &mut Vec<w::AudioOutputEvent>) {
        while let Some(slot) = self
            .slots
            .get_mut(self.next as usize)
            .and_then(|slot| slot.take())
        {
            let sequence_no = self.next;
            self.next += 1;
            if let Slot::Audio(pcm) = slot {
                self.emit_audio(sequence_no, pcm, emitted);
            }
        }
    }

    fn emit_audio(&self, sequence_no: u32, pcm: Vec<u8>, emitted: &mut Vec<w::AudioOutputEvent>) {
        if pcm.is_empty() {
            return;
        }
        let mut offset = 0usize;
        while offset < pcm.len() {
            let end = (offset + AUDIO_CHUNK_BYTES).min(pcm.len());
            emitted.push(audio_event(w::AudioChunkEnvelope {
                utterance_id: self.utterance_id.clone(),
                generation_id: self.generation_id,
                sequence_no,
                pcm_data: pcm[offset..end].to_vec(),
                sample_rate: SAMPLE_RATE,
                is_final_chunk: end == pcm.len(),
                ..Default::default()
            }));
            offset = end;
        }
    }

    fn emit_finished(&mut self, emitted: &mut Vec<w::AudioOutputEvent>) {
        let Some(total_sentences) = self.finished else {
            return;
        };
        if self.sent_finished || self.next < total_sentences {
            return;
        }
        self.sent_finished = true;
        emitted.push(audio_event(w::UtteranceFinished {
            utterance_id: self.utterance_id.clone(),
            generation_id: self.generation_id,
            total_sentences,
            ..Default::default()
        }));
    }
}

/// One utterance's ordering state in the worker.
struct Work {
    companion: String,
    generation_id: u64,
    emitter: Emitter,
    pending: VecDeque<Sentence>,
    in_flight: usize,
}

impl Work {
    fn new(sentence: &Sentence) -> Self {
        Self {
            companion: sentence.companion.clone(),
            generation_id: sentence.generation_id,
            emitter: Emitter::new(sentence.utterance_id.clone(), sentence.generation_id),
            pending: VecDeque::new(),
            in_flight: 0,
        }
    }
}

type Completed = (String, u32, HttpResult<Vec<u8>>);

/// Runs the configured sentences concurrently and releases their results in
/// order. It owns the only HTTP work of the pipeline.
struct Worker {
    shared: Arc<Mutex<State>>,
    completions: mpsc::UnboundedSender<Completed>,
    engines: HashMap<String, Arc<dyn TtsEngine>>,
    work: HashMap<String, Work>,
    in_flight: usize,
}

impl Worker {
    fn new(shared: Arc<Mutex<State>>) -> (Self, mpsc::UnboundedReceiver<Completed>) {
        let (completions, receiver) = mpsc::unbounded_channel();
        let worker = Self {
            shared,
            completions,
            engines: HashMap::new(),
            work: HashMap::new(),
            in_flight: 0,
        };
        (worker, receiver)
    }

    async fn run(
        mut self,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        mut completed: mpsc::UnboundedReceiver<Completed>,
    ) {
        let mut open = true;
        loop {
            if !open && self.in_flight == 0 {
                break;
            }
            tokio::select! {
                job = jobs.recv(), if open => match job {
                    Some(job) => self.accept(job).await,
                    None => open = false,
                },
                completion = completed.recv() => {
                    if let Some((utterance_id, sequence_no, outcome)) = completion {
                        self.complete(utterance_id, sequence_no, outcome).await;
                    }
                }
            }
        }
    }

    async fn accept(&mut self, job: Job) {
        match job {
            Job::Sentence(sentence) => {
                let utterance_id = sentence.utterance_id.clone();
                self.work
                    .entry(utterance_id.clone())
                    .or_insert_with(|| Work::new(&sentence))
                    .pending
                    .push_back(sentence);
                self.pump(&utterance_id);
            }
            Job::Finish {
                utterance_id,
                total_sentences,
            } => {
                let Some((companion, generation_id, events)) =
                    self.events(&utterance_id, |work| work.emitter.finish(total_sentences))
                else {
                    return;
                };
                self.send(&companion, generation_id, events).await;
                self.retire_if_done(&utterance_id);
            }
            Job::Invalidate { utterance_id } => {
                self.work.remove(&utterance_id);
            }
        }
    }

    /// Applies `update` to one utterance's worker state and returns the
    /// companion and generation needed to deliver its events.
    fn events(
        &mut self,
        utterance_id: &str,
        update: impl FnOnce(&mut Work) -> Vec<w::AudioOutputEvent>,
    ) -> Option<(String, u64, Vec<w::AudioOutputEvent>)> {
        let work = self.work.get_mut(utterance_id)?;
        let events = update(work);
        Some((work.companion.clone(), work.generation_id, events))
    }

    fn pump(&mut self, utterance_id: &str) {
        while let Some(work) = self.work.get_mut(utterance_id) {
            if work.in_flight >= MAX_SYNTHESIS_SENTENCES {
                break;
            }
            let Some(sentence) = work.pending.pop_front() else {
                break;
            };
            work.in_flight += 1;
            self.in_flight += 1;
            let engine = self.engine(&sentence.source);
            let sequence_no = sentence.sequence_no;
            let completed = self.completions.clone();
            let id = utterance_id.to_owned();
            tokio::spawn(async move {
                let outcome = synthesize(&*engine, sentence.text, sentence.voice).await;
                let _ = completed.send((id, sequence_no, outcome));
            });
        }
    }

    async fn complete(
        &mut self,
        utterance_id: String,
        sequence_no: u32,
        outcome: HttpResult<Vec<u8>>,
    ) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let Some((companion, generation_id, events)) = self.events(&utterance_id, |work| {
            work.in_flight = work.in_flight.saturating_sub(1);
            work.emitter.offer(sequence_no, outcome)
        }) else {
            return;
        };
        self.send(&companion, generation_id, events).await;
        self.retire_if_done(&utterance_id);
        self.pump(&utterance_id);
    }

    fn retire_if_done(&mut self, utterance_id: &str) {
        if self
            .work
            .get(utterance_id)
            .is_some_and(|work| work.emitter.sent_finished)
        {
            self.work.remove(utterance_id);
        }
    }

    async fn send(&self, companion: &str, generation_id: u64, events: Vec<w::AudioOutputEvent>) {
        for event in events {
            deliver(&self.shared, companion, generation_id, event).await;
        }
    }

    fn engine(&mut self, source: &TtsSource) -> Arc<dyn TtsEngine> {
        match &source.key {
            Some(key) => self
                .engines
                .entry(key.clone())
                .or_insert_with(|| Arc::clone(&source.engine))
                .clone(),
            None => Arc::clone(&source.engine),
        }
    }
}

/// Sends `event` to the companion's audio-owning client when its generation is
/// still current. The send waits for room: dropping an audio chunk would leave
/// the client waiting for a `sequence_no` that never closes.
async fn deliver(
    shared: &Mutex<State>,
    companion: &str,
    generation_id: u64,
    event: w::AudioOutputEvent,
) -> bool {
    let sender = {
        let state = lock(shared);
        let Some(owner) = state.owners.get(companion) else {
            return false;
        };
        if owner.utterance_id.is_none() || owner.generation_id != generation_id {
            return false;
        }
        let Some(stream) = state.streams.get(&owner.client_id) else {
            return false;
        };
        stream.audio.clone()
    };
    sender.send(event).await.is_ok()
}

/// One sentence's synthesis with the design's timeout. A failure or a timeout
/// becomes `SentenceSkipped`, never silence.
async fn synthesize(engine: &dyn TtsEngine, text: String, voice: String) -> HttpResult<Vec<u8>> {
    match tokio::time::timeout(SYNTHESIS_TIMEOUT, engine.synthesize(text, voice)).await {
        Err(_) => Err("the TTS connection timed out".into()),
        Ok(Err(error)) => Err(error),
        Ok(Ok((pcm, sample_rate))) => wire_pcm(pcm, sample_rate),
    }
}

fn audio_event(message: impl Into<w::audio_output_event::Event>) -> w::AudioOutputEvent {
    w::AudioOutputEvent {
        event: Some(message.into()),
        ..Default::default()
    }
}

fn flush_event(
    utterance_id: &str,
    flush_generation_id: u64,
    reason: w::FlushReason,
) -> w::AudioOutputEvent {
    audio_event(w::FlushControl {
        utterance_id: utterance_id.to_owned(),
        flush_generation_id,
        reason: reason.into(),
        ..Default::default()
    })
}

fn spawn_worker(shared: Arc<Mutex<State>>, receiver: mpsc::UnboundedReceiver<Job>) {
    let spawned = std::thread::Builder::new()
        .name("ene-voice-tts".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("the voice worker could not build its runtime");
                return;
            };
            let (worker, completed) = Worker::new(shared);
            runtime.block_on(worker.run(receiver, completed));
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "the voice worker thread did not start");
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InputBinding {
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
}

impl InputBinding {
    /// Whether `chunk` belongs to this stream. A chunk may omit the ids; the
    /// stream's binding then stays authoritative.
    pub(crate) fn accepts(&self, chunk: &w::AudioInputChunk) -> bool {
        (chunk.client_id.is_empty() || chunk.client_id == self.client_id)
            && (chunk.activation_revision == 0
                || chunk.activation_revision == self.activation_revision)
    }
}

/// Binds a new microphone stream to the client's current activation.
pub(crate) fn bind_input(world: &World, chunk: &w::AudioInputChunk) -> Result<InputBinding> {
    if chunk.client_id.is_empty() {
        return Err(HostError::invalid("client_id is required for audio input"));
    }
    db::validate_id(&chunk.client_id)?;
    let Some(store) = world.get_resource::<Store>() else {
        return Err(HostError::unavailable("the host state is not available"));
    };
    if store.client(&chunk.client_id)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let mut active = false;
    let mut matched = false;
    for activation in store.activations()? {
        if activation.client_id.as_deref() != Some(chunk.client_id.as_str()) {
            continue;
        }
        active = true;
        if activation.revision == chunk.activation_revision {
            matched = true;
            break;
        }
    }
    if !active {
        return Err(HostError::conflict(
            "the client is not the active client of any companion",
        ));
    }
    if !matched {
        return Err(HostError::conflict(
            "client_moved: the activation revision is stale",
        ));
    }
    Ok(InputBinding {
        client_id: chunk.client_id.clone(),
        activation_revision: chunk.activation_revision,
    })
}

/// Whether the activation that bound the stream is still current.
pub(crate) fn is_still_active(world: &World, binding: &InputBinding) -> bool {
    let Some(store) = world.get_resource::<Store>() else {
        return false;
    };
    store.activations().is_ok_and(|activations| {
        activations.iter().any(|activation| {
            activation.client_id.as_deref() == Some(binding.client_id.as_str())
                && activation.revision == binding.activation_revision
        })
    })
}

/// A live `SubscribeAudioOutput` stream.
pub(crate) struct AudioSubscription {
    pub(crate) subscription_id: String,
    pub(crate) receiver: mpsc::Receiver<w::AudioOutputEvent>,
    pub(crate) control: mpsc::UnboundedReceiver<w::AudioOutputEvent>,
}

/// Subscribes the active audio-owning client of a companion.
pub(crate) fn subscribe_audio(
    world: &mut World,
    request: w::SubscribeAudioRequest,
) -> Result<AudioSubscription> {
    if request.client_id.is_empty() {
        return Err(HostError::invalid("client_id is required"));
    }
    db::validate_id(&request.client_id)?;
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Err(HostError::unavailable("the host state is not available"));
    };
    if store.client(&request.client_id)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let activations = store.activations()?;
    if !activations
        .iter()
        .any(|activation| activation.client_id.as_deref() == Some(request.client_id.as_str()))
    {
        return Err(HostError::conflict(
            "the client is not the active client of any companion",
        ));
    }
    // One client owns the playback; it may host several companions.
    let owned: Vec<String> = activations
        .iter()
        .filter(|activation| {
            activation.client_id.as_deref() == Some(request.client_id.as_str())
                && activation.revision == request.activation_revision
        })
        .map(|activation| activation.companion_id.clone())
        .collect();
    if owned.is_empty() {
        return Err(HostError::conflict(
            "client_moved: the activation revision is stale",
        ));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::internal("the voice plugin is not installed"));
    };
    let subscription = voice.register(&request.client_id);
    for companion_id in owned {
        voice.publish(
            &companion_id,
            &request.client_id,
            request.activation_revision,
        );
    }
    Ok(subscription)
}

/// Closes one client's audio stream. Playback ownership is kept: a reconnect
/// with the same activation revision recovers, a move does not.
pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.unregister(client, subscription);
    }
}

/// Records a client's playback report. Only the current owner's report for the
/// current generation releases the floor; every other report is discarded.
pub(crate) fn report_playback(
    world: &mut World,
    report: w::PlaybackStatusReport,
) -> Result<w::ReportPlaybackStatusResponse> {
    let discarded = || w::ReportPlaybackStatusResponse {
        acknowledged: false,
        ..Default::default()
    };
    let acknowledged = || w::ReportPlaybackStatusResponse {
        acknowledged: true,
        ..Default::default()
    };
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(discarded());
    };
    db::validate_id(&report.client_id)?;
    if store.client(&report.client_id)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Ok(discarded());
    };
    let Some((companion, owner)) = voice.owner_of(&report.utterance_id) else {
        return Ok(discarded());
    };
    if owner.client_id != report.client_id
        || owner.activation_revision != report.activation_revision
        || owner.generation_id != report.generation_id
    {
        return Ok(discarded());
    }
    // A moved client's report never releases the new generation's floor.
    let current = store
        .activation(&companion)?
        .map(|activation| activation.revision)
        .unwrap_or(0);
    if current != owner.activation_revision {
        return Ok(discarded());
    }
    let status = report.status.as_known();
    let completed = matches!(
        status,
        Some(w::playback_status_report::Status::PLAYBACK_COMPLETED)
    );
    let ended = completed
        || matches!(
            status,
            Some(
                w::playback_status_report::Status::PLAYBACK_INTERRUPTED
                    | w::playback_status_report::Status::PLAYBACK_FAILED
            )
        );
    if !ended {
        return Ok(discarded());
    }
    // Interrupted or failed playback frees the floor without claiming that
    // the words were delivered.
    let total_sentences = owner.total_sentences;
    voice.release(&companion);
    if completed {
        broadcast_speech_completed(world, &companion, &report, total_sentences);
    }
    Ok(acknowledged())
}

/// Broadcasts the conversation relay signal: the companion finished speaking.
fn broadcast_speech_completed(
    world: &mut World,
    companion: &str,
    report: &w::PlaybackStatusReport,
    total_sentences: Option<u32>,
) {
    let document = w::JsonDocument {
        schema: "ene.speech_completed.v1".into(),
        json: serde_json::json!({
            "companion_id": companion,
            "client_id": report.client_id,
            "utterance_id": report.utterance_id,
            "generation_id": report.generation_id,
            "total_sentences": total_sentences,
            "completed_sentence_count": report.completed_sentence_count,
            "completed_pcm_bytes": report.completed_pcm_bytes,
            "played_sequence_nos": report.played_sequence_nos,
            "skipped_sequence_nos": report.skipped_sequence_nos,
        })
        .to_string(),
        ..Default::default()
    };
    let Some(mut notifier) = world.get_resource_mut::<Notifier>() else {
        return;
    };
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "speech_completed".into(),
            payload: Some(document).into(),
            ..Default::default()
        })),
    );
}

/// Broadcasts a final transcript to every active companion. The transcription
/// id becomes the request id, so work delegation can bind to it.
pub(crate) fn accept_transcription(
    world: &mut World,
    transcription_id: &str,
    text: &str,
    client_id: &str,
) -> Result<usize> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    if let Some(voice) = world.get_resource::<Voice>()
        && !voice.accept_transcription(transcription_id)
    {
        return Ok(0);
    }
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(0);
    };
    let companions: Vec<String> = store
        .activations()?
        .into_iter()
        .filter(|activation| activation.client_id.is_some())
        .map(|activation| activation.companion_id)
        .collect();
    let author = Author {
        request: Some(RequestId(transcription_id.to_owned())),
        ..Author::user()
    };
    let mut delivered = 0usize;
    for companion in companions {
        let Some(agent) = agent::agent_of(world, &companion) else {
            continue;
        };
        if let Some(mut conversation) = world.get_mut::<Conversation>(agent) {
            conversation.add_aside(Aside::new(
                "ene.transcription",
                Author::plugin("ene-host"),
                serde_json::json!({
                    "transcription_id": transcription_id,
                    "client_id": client_id,
                }),
            ));
        }
        world.trigger(
            Deliver::new(agent, text.to_owned(), DeliveryMode::Steer).with_author(author.clone()),
        );
        delivered += 1;
    }
    Ok(delivered)
}

/// Called from `Stage::Review` for every text delta of an ordinary companion
/// turn (the lead wires this into `host::agent::stream_deltas`).
pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    if !voice.owns(companion) {
        // No client owns this companion's audio: the host stays text-only.
        return;
    }
    if voice.streaming(companion).is_none() {
        let Ok(source) = tts_source(world) else {
            return;
        };
        if let Some(holder) = voice.floor_holder()
            && holder != companion
        {
            // The first companion to speak keeps the floor; a peer's turn
            // output is stopped before it can be recorded.
            if let Some(agent) = agent::agent_of(world, companion) {
                world.trigger(StopTurn {
                    entity: agent,
                    reason: Some("speech floor was taken by a peer".to_owned()),
                });
            }
            return;
        }
        let voice_name = world
            .get_resource::<Store>()
            .and_then(|store| store.companion(companion).ok().flatten())
            .and_then(|companion| companion.voice_identifier)
            .unwrap_or_default();
        if !voice.begin(companion, source, voice_name) {
            return;
        }
    }
    voice.speak(companion, text);
}

/// Called when a turn settles (the lead wires this into
/// `host::agent::turn_settled`): flush the unterminated remainder and finish
/// the utterance's send side.
pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.end_turn(companion);
    }
}

/// Flushes the companion's playback. A new utterance or a move invalidates the
/// older generation; the client discards everything up to
/// `flush_generation_id`.
pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::not_found(
            "no client owns this companion's audio output",
        ));
    };
    voice.flush(companion, reason)
}

/// Called when the active client moved (the lead wires this into
/// `host::client::activate`): the old client is flushed with
/// `FLUSH_REASON_CLIENT_MOVED` and loses playback ownership.
pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str, revision: u64) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        voice.client_moved(companion, client, revision);
    }
}

/// Releases the floor of a companion whose client never reported playback.
fn expire_playback(world: &mut World) {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    let now = Instant::now();
    let expired: Vec<String> = {
        let state = voice.lock();
        state
            .owners
            .iter()
            .filter(|(_, owner)| owner.deadline.is_some_and(|deadline| deadline <= now))
            .map(|(companion, _)| companion.clone())
            .collect()
    };
    for companion in expired {
        tracing::debug!(%companion, "playback did not report; releasing the speech floor");
        voice.release(&companion);
    }
}

/// Registers the voice resources, the synthesis worker and the deadline
/// system. The lead adds `voice::VoicePlugin` to the host plugin list.
pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        let (voice, receiver) = Voice::new();
        spawn_worker(voice.shared(), receiver);
        app.insert_resource(voice)
            .add_systems(Update, expire_playback);
    }
}

/// Test-only engine injection: the unit tests never build a provider client.
#[cfg(test)]
#[derive(Resource, Default)]
struct TestEngines {
    tts: Option<Arc<dyn TtsEngine>>,
    stt: Option<Arc<dyn SttEngine>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::clients::ClientDeclaration;

    /// Deterministic PCM, optionally failing; the tests never touch the network.
    struct FakeTts {
        fail: bool,
    }

    impl TtsEngine for FakeTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                if self.fail {
                    return Err("mock tts failed".into());
                }
                Ok((
                    text.bytes()
                        .flat_map(|byte| i16::from(byte).to_le_bytes())
                        .collect(),
                    SAMPLE_RATE,
                ))
            })
        }
    }

    struct FakeStt;

    impl SttEngine for FakeStt {
        fn transcribe<'a>(&'a self, pcm: Vec<u8>, _: u32) -> BoxFuture<'a, HttpResult<String>> {
            Box::pin(async move { Ok(format!("{} bytes", pcm.len())) })
        }
    }

    fn current_owner(world: &World, companion: &str) -> Owner {
        world
            .resource::<Voice>()
            .lock()
            .owners
            .get(companion)
            .unwrap()
            .clone()
    }

    fn test_world() -> (World, mpsc::UnboundedReceiver<Job>) {
        let (voice, jobs) = Voice::new();
        let mut world = World::new();
        world.insert_resource(voice);
        world.insert_resource(TestEngines {
            tts: Some(Arc::new(FakeTts { fail: false })),
            stt: Some(Arc::new(FakeStt)),
        });
        (world, jobs)
    }

    fn store_with_client() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        store
            .register_client(&ClientDeclaration {
                id: "c1",
                name: "desk",
                platform: "linux",
                capabilities: &[],
                specs: &[],
                expected_revision: 0,
            })
            .unwrap();
        store.activate("ene", Some("c1"), 0).unwrap();
        (root, store)
    }

    fn chunks(events: &[w::AudioOutputEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|event| match &event.event {
                Some(w::audio_output_event::Event::Chunk(chunk)) => Some(chunk.sequence_no),
                _ => None,
            })
            .collect()
    }

    fn finished(events: &[w::AudioOutputEvent]) -> Option<u32> {
        events.iter().find_map(|event| match &event.event {
            Some(w::audio_output_event::Event::Finished(finished)) => {
                Some(finished.total_sentences)
            }
            _ => None,
        })
    }

    #[test]
    fn sentences_split_on_the_design_terminators() {
        let mut buffer = String::from("おはよう。今日はいい天気ですね！明日は？\n");
        assert_eq!(
            split_sentences(&mut buffer),
            vec!["おはよう。", "今日はいい天気ですね！", "明日は？"]
        );
        assert!(buffer.is_empty());

        let mut buffer = String::from("まだ終わっていない");
        assert!(split_sentences(&mut buffer).is_empty());
        assert_eq!(
            flush_sentences(&mut buffer).as_deref(),
            Some("まだ終わっていない")
        );

        // A long unpunctuated run is still spoken, in sentence-sized pieces.
        let mut buffer = "あ".repeat(MAX_SENTENCE_CHARS + 5);
        let sentences = split_sentences(&mut buffer);
        assert_eq!(sentences.len(), 1);
        assert_eq!(sentences[0].chars().count(), MAX_SENTENCE_CHARS);
        assert_eq!(buffer.chars().count(), 5);
    }

    #[test]
    fn out_of_order_synthesis_is_released_in_sequence_order() {
        let mut emitter = Emitter::new("u".into(), 7);
        // The second sentence finishes first: nothing is sent yet.
        assert!(emitter.offer(1, Ok(vec![2, 0])).is_empty());
        let events = emitter.offer(0, Ok(vec![1, 0]));
        assert_eq!(chunks(&events), [0, 1]);
        assert!(events.iter().all(|event| matches!(
            &event.event,
            Some(w::audio_output_event::Event::Chunk(chunk)) if chunk.is_final_chunk
        )));
        assert_eq!(finished(&emitter.finish(2)), Some(2));
    }

    #[test]
    fn a_skipped_sentence_unblocks_the_next_one() {
        let mut emitter = Emitter::new("u".into(), 7);
        let events = emitter.offer(0, Err("timed out".into()));
        assert!(matches!(
            events.as_slice(),
            [w::AudioOutputEvent { event: Some(w::audio_output_event::Event::Skipped(skipped)), .. }]
                if skipped.sequence_no == 0
        ));
        assert_eq!(chunks(&emitter.offer(1, Ok(vec![0, 0]))), [1]);
        assert_eq!(finished(&emitter.finish(2)), Some(2));
    }

    #[tokio::test]
    async fn a_flush_invalidates_the_older_generation() {
        let (mut world, _jobs) = test_world();
        let subscription = world.resource::<Voice>().register("c1");
        let mut events = subscription.control;
        let _audio = subscription.receiver;
        world.resource::<Voice>().publish("ene", "c1", 1);
        on_stream_text(&mut world, "ene", "ひとつめ。");
        let first = current_owner(&world, "ene").generation_id;
        assert_eq!(first, 1);

        flush(&mut world, "ene", w::FlushReason::BargeIn).unwrap();
        match events.try_recv().expect("a flush reaches the client").event {
            Some(w::audio_output_event::Event::Flush(flush)) => {
                assert_eq!(flush.flush_generation_id, first);
                assert_eq!(flush.reason, w::FlushReason::BargeIn);
            }
            other => panic!("expected a flush, got {other:?}"),
        }
        // The delivery gate drops audio of the invalidated generation.
        let stale = audio_event(w::AudioChunkEnvelope {
            utterance_id: "one".into(),
            generation_id: first,
            ..Default::default()
        });
        let voice = world.resource::<Voice>().shared();
        assert!(!deliver(&voice, "ene", first, stale).await);

        on_stream_text(&mut world, "ene", "ふたつめ。");
        let second = current_owner(&world, "ene").generation_id;
        assert!(second > first);
        let current = audio_event(w::AudioChunkEnvelope {
            utterance_id: "two".into(),
            generation_id: second,
            ..Default::default()
        });
        assert!(deliver(&voice, "ene", second, current).await);
    }

    #[test]
    fn a_stale_playback_report_is_ignored() {
        let (_root, store) = store_with_client();
        let (mut world, _jobs) = test_world();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        world.resource::<Voice>().publish("ene", "c1", 1);
        on_stream_text(&mut world, "ene", "こんにちは。");
        on_turn_end(&mut world, "ene");
        let owner = current_owner(&world, "ene");
        let utterance_id = owner.utterance_id.clone().unwrap();
        assert_eq!(owner.total_sentences, Some(1));
        assert!(owner.deadline.is_some());

        let report = |generation_id| w::PlaybackStatusReport {
            utterance_id: utterance_id.clone(),
            generation_id,
            status: w::playback_status_report::Status::PLAYBACK_COMPLETED.into(),
            completed_sentence_count: 1,
            client_id: "c1".into(),
            activation_revision: 1,
            ..Default::default()
        };
        assert!(
            !report_playback(&mut world, report(owner.generation_id + 1))
                .unwrap()
                .acknowledged
        );
        assert!(current_owner(&world, "ene").utterance_id.is_some());

        let mut events = world.resource::<Notifier>().subscribe();
        assert!(
            report_playback(&mut world, report(owner.generation_id))
                .unwrap()
                .acknowledged
        );
        assert!(current_owner(&world, "ene").utterance_id.is_none());
        assert!(world.resource::<Voice>().floor_holder().is_none());
        world.resource_mut::<Notifier>().flush();
        assert!(matches!(
            events.try_recv().expect("speech_completed is broadcast").event,
            Some(w::notification::Event::Generic(event)) if event.kind == "speech_completed"
        ));
    }

    #[test]
    fn audio_bound_to_a_moved_activation_is_discarded() {
        let (_root, store) = store_with_client();
        let mut world = World::new();
        world.insert_resource(store.clone());
        let chunk = w::AudioInputChunk {
            client_id: "c1".into(),
            activation_revision: 1,
            ..Default::default()
        };
        let binding = bind_input(&world, &chunk).unwrap();
        assert!(binding.accepts(&chunk) && is_still_active(&world, &binding));

        store.activate("ene", Some("c1"), 1).unwrap();
        assert!(!is_still_active(&world, &binding));
        let moved = w::AudioInputChunk {
            activation_revision: 2,
            ..chunk.clone()
        };
        assert!(!binding.accepts(&moved));
        assert!(matches!(
            bind_input(&world, &chunk),
            Err(HostError::Conflict(_))
        ));
    }

    #[test]
    fn a_host_without_a_connection_reports_the_capability_as_missing() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Credentials::default());
        assert_eq!(tts_source(&world).err(), Some(SetupError::Missing));
        assert_eq!(stt_source(&world).err(), Some(SetupError::Missing));
    }

    #[test]
    fn the_stt_wav_header_names_the_wire_format() {
        let wav = wav_wrap(&[0u8; 8], 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(le16(&wav, 20), 1);
        assert_eq!(le16(&wav, 22), 1);
        assert_eq!(le32(&wav, 24), 16_000);
        assert_eq!(wav.len(), 52);
    }

    #[tokio::test]
    async fn fake_engines_drive_both_traits_without_a_network() {
        let pcm = synthesize(&FakeTts { fail: false }, "ab".into(), String::new())
            .await
            .unwrap();
        assert_eq!(pcm.len(), 4);
        assert!(
            synthesize(&FakeTts { fail: true }, "あ".into(), String::new())
                .await
                .is_err()
        );
        assert_eq!(
            FakeStt.transcribe(vec![0; 480], SAMPLE_RATE).await.unwrap(),
            "480 bytes"
        );
    }

    #[tokio::test]
    async fn the_worker_orders_audio_and_finishes_the_utterance() {
        let (voice, jobs) = Voice::new();
        let mut events = voice.register("c1").receiver;
        voice.publish("ene", "c1", 1);
        {
            let mut state = voice.lock();
            state.owners.get_mut("ene").unwrap().utterance_id = Some("u1".into());
        }
        let (worker, completed) = Worker::new(voice.shared());
        let worker = tokio::spawn(worker.run(jobs, completed));
        let source = TtsSource {
            key: None,
            engine: Arc::new(FakeTts { fail: false }),
        };
        // The second sentence is queued first: the ordering gate still emits
        // sequence 0 before sequence 1.
        for sequence_no in [1u32, 0] {
            voice.job(Job::Sentence(Sentence {
                companion: "ene".into(),
                utterance_id: "u1".into(),
                generation_id: 0,
                sequence_no,
                text: format!("s{sequence_no}"),
                source: source.clone(),
                voice: String::new(),
            }));
        }
        voice.job(Job::Finish {
            utterance_id: "u1".into(),
            total_sentences: 2,
        });
        drop(voice);
        let collected = tokio::time::timeout(Duration::from_secs(5), async {
            let mut collected = Vec::new();
            while let Some(event) = events.recv().await {
                let done = finished(std::slice::from_ref(&event)) == Some(2);
                collected.push(event);
                if done {
                    break;
                }
            }
            collected
        })
        .await
        .expect("the worker finishes the utterance");
        assert_eq!(chunks(&collected), [0, 1]);
        assert_eq!(finished(&collected), Some(2));
        worker.await.unwrap();
    }
}
