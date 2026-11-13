//! Host-side voice pipeline: recognition input, sentence-level synthesis
//! output, playback ownership and the speech floor (docs/design/voice-avatar.md
//! §4–§5, docs/api.md §6–§7). The host thread keeps state; one worker thread
//! owns a Tokio runtime, runs at most three syntheses per utterance and
//! releases the audio in `sequence_no` order. Recognition runs in the Connect
//! layer. Without a TTS/STT connection the capability is unavailable.
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
use futures_util::StreamExt;
use rig_core::conversation::{Aside, Author, RequestId};
use rig_ecs::prelude::*;
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    net::{IpAddr, ToSocketAddrs},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// The wire standard: 24 kHz, 16-bit signed little-endian, one channel.
pub(crate) const SAMPLE_RATE: u32 = 24_000;
pub(crate) const STT_SAMPLE_RATES: [u32; 2] = [24_000, 16_000];
const MAX_SYNTHESIS_SENTENCES: usize = 3;
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const STT_TIMEOUT: Duration = Duration::from_secs(30);
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_SENTENCE_CHARS: usize = 240;
pub(crate) const STT_SILENCE_WINDOW: Duration = Duration::from_millis(700);
pub(crate) const MIN_STT_BYTES: usize = 3_200;
pub(crate) const MAX_STT_BYTES: usize = 48_000 * 20;
const PLAYBACK_GRACE: Duration = Duration::from_secs(3);
const TRANSCRIPTION_MEMORY: usize = 128;
/// Bounds one utterance's queued sentences and the worker's emitter slot map.
const MAX_SENTENCES: u32 = 64;
/// Audio events buffered per client before that stalled owner is flushed.
const AUDIO_QUEUE: usize = 32;
/// The largest TTS/STT response body the host accepts.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const VOICE_UNAVAILABLE: &str = "voice requires a configured TTS/STT connection";

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type HttpResult<T> = std::result::Result<T, String>;

/// Why a configured voice capability is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupError {
    Missing,
    Disabled(String),
    Broken(String),
    Unsupported(String),
}

/// The synthesis seam: mono 16-bit PCM and its sample rate.
pub(crate) trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>>;
}

/// The recognition seam: the transcript of one utterance.
pub(crate) trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtsProtocol {
    OpenAi,
    Voicevox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SttProtocol {
    OpenAi,
    Raw,
}

#[derive(Clone)]
pub(crate) struct TtsSource {
    key: Option<String>,
    engine: Arc<dyn TtsEngine>,
}

#[derive(Deserialize, Default)]
struct Options {
    protocol: Option<String>,
    model: Option<String>,
    voice: Option<String>,
    language: Option<String>,
    path: Option<String>,
    audio_query_path: Option<String>,
    synthesis_path: Option<String>,
    speaker: Option<u64>,
    speed: Option<f64>,
    timeout_ms: Option<u64>,
}

fn engine_setup(
    connection: &Connection,
    credentials: Option<&Credentials>,
    label: &str,
) -> std::result::Result<(String, Option<String>, Options), SetupError> {
    let endpoint = connection.endpoint().clone();
    let base = base_url(&endpoint, label)?;
    let options = options(&endpoint, label)?;
    let secret = credential(connection, credentials, &base, label)?;
    Ok((base, secret, options))
}

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
        let (base, secret, options) = engine_setup(&connection, credentials, "TTS")?;
        let protocol = match nonempty(options.protocol).as_deref().unwrap_or(
            if endpoint.adapter == "voicevox" {
                "voicevox"
            } else {
                "openai"
            },
        ) {
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
        Ok(Self {
            client: http_client(options.timeout_ms, 20_000),
            base,
            protocol,
            model: nonempty(options.model).unwrap_or_else(|| "tts-1".into()),
            voice: nonempty(options.voice)
                .or(default_voice)
                .unwrap_or_else(|| "alloy".into()),
            path: path(options.path, "/audio/speech"),
            query_path: path(options.audio_query_path, "/audio_query"),
            synthesis_path: path(options.synthesis_path, "/synthesis"),
            speaker: options.speaker.unwrap_or(1).min(u64::from(u32::MAX)) as u32,
            speed: options.speed.map(|speed| speed as f32),
            secret,
        })
    }

    fn client(&self) -> HttpResult<&reqwest::Client> {
        self.client.as_ref().map_err(Clone::clone)
    }

    async fn openai(&self, text: String, voice: String) -> HttpResult<(Vec<u8>, u32)> {
        ensure_target(&self.base, "TTS").await?;
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
        Ok((read_body(response, "TTS").await?, SAMPLE_RATE))
    }

    async fn voicevox(&self, text: String) -> HttpResult<(Vec<u8>, u32)> {
        ensure_target(&self.base, "TTS").await?;
        let encoded: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
        let call = self.client()?.post(format!(
            "{}{}?text={encoded}&speaker={}",
            self.base, self.query_path, self.speaker
        ));
        let query = post(call, self.secret.as_deref(), "TTS").await?;
        let query = read_body(query, "TTS").await?;
        let mut body: serde_json::Value = serde_json::from_slice(&query)
            .map_err(|_| "the TTS endpoint answered a non-JSON audio query".to_owned())?;
        if let Some(speed) = self.speed {
            body["speedScale"] = serde_json::json!(speed);
        }
        let call = self
            .client()?
            .post(format!(
                "{}{}?speaker={}",
                self.base, self.synthesis_path, self.speaker
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let response = post(call, self.secret.as_deref(), "TTS").await?;
        parse_wav(&read_body(response, "TTS").await?)
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
        let (base, secret, options) = engine_setup(&connection, credentials, "STT")?;
        let protocol = match nonempty(options.protocol).as_deref().unwrap_or("openai") {
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
        Ok(Self {
            client: http_client(options.timeout_ms, 30_000),
            base,
            protocol,
            path: path(options.path, "/audio/transcriptions"),
            model: nonempty(options.model).unwrap_or_else(|| "whisper-1".into()),
            language: nonempty(options.language).or_else(|| hints.first().cloned()),
            secret,
        })
    }

    async fn request(&self, pcm: Vec<u8>, sample_rate: u32) -> HttpResult<reqwest::Response> {
        ensure_target(&self.base, "STT").await?;
        let client = self.client.as_ref().map_err(Clone::clone)?;
        let url = format!("{}{}", self.base, self.path);
        let wav = wav_wrap(&pcm, sample_rate);
        match self.protocol {
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
                .await
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
                .await
            }
        }
    }
}

impl SttEngine for HttpSttEngine {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move {
            let response = self.request(pcm, sample_rate).await?;
            let body = read_body(response, "STT").await?;
            match self.protocol {
                SttProtocol::OpenAi => {
                    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
                        "the STT endpoint answered a non-JSON transcript".to_owned()
                    })?;
                    Ok(value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned())
                }
                SttProtocol::Raw => String::from_utf8(body)
                    .map(|text| text.trim().to_owned())
                    .map_err(|_| "the STT endpoint answered a non-UTF-8 transcript".to_owned()),
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

/// Reads a response body with a hard cap so one endpoint cannot exhaust host memory.
async fn read_body(response: reqwest::Response, label: &str) -> HttpResult<Vec<u8>> {
    let too_large = || format!("the {label} endpoint body exceeds {MAX_BODY_BYTES} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("the {label} endpoint body: {error}"))?;
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

fn options<T: DeserializeOwned + Default>(
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

fn path(value: Option<String>, default: &str) -> String {
    match nonempty(value) {
        Some(path) if path.starts_with('/') => path,
        Some(path) => format!("/{path}"),
        None => default.to_owned(),
    }
}

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

/// Validates the configured base URL without network access: plain HTTP is
/// loopback-only and a literal address must be public.
fn base_url(endpoint: &Endpoint, label: &str) -> std::result::Result<String, SetupError> {
    let base = endpoint.endpoint.trim().trim_end_matches('/');
    let url = url::Url::parse(base)
        .map_err(|_| SetupError::Broken(format!("the {label} connection endpoint is not a URL")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&url) => {}
        "http" => {
            return Err(SetupError::Broken(format!(
                "the {label} connection endpoint must use https outside loopback"
            )));
        }
        _ => {
            return Err(SetupError::Broken(format!(
                "the {label} connection endpoint must be http(s)"
            )));
        }
    }
    if let Some(ip) = literal_ip(&url)
        && !is_loopback(&url)
        && blocked(ip)
    {
        return Err(SetupError::Broken(format!(
            "the {label} connection endpoint is not a public address"
        )));
    }
    Ok(base.to_owned())
}

/// Rejects a target that could reach the host or its private network. The host
/// resolves it before building the request, so a rejected URL never receives
/// credentials.
async fn ensure_target(base: &str, label: &str) -> HttpResult<()> {
    let url = url::Url::parse(base)
        .map_err(|_| format!("the {label} connection endpoint is not a URL"))?;
    let loopback = is_loopback(&url);
    match url.scheme() {
        "https" => {}
        "http" if loopback => return Ok(()),
        "http" => {
            return Err(format!(
                "the {label} connection endpoint must use https outside loopback"
            ));
        }
        _ => return Err(format!("the {label} connection endpoint must be http(s)")),
    }
    if loopback {
        return Ok(());
    }
    if let Some(ip) = literal_ip(&url) {
        return if blocked(ip) {
            Err(format!("the {label} endpoint is not a public address"))
        } else {
            Ok(())
        };
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("the {label} connection endpoint has no host"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| format!("the {label} connection endpoint has no port"))?;
    let addresses = tokio::task::spawn_blocking(move || {
        (host.as_str(), port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<_>>())
    })
    .await
    .map_err(|error| format!("the {label} endpoint address lookup: {error}"))?
    .map_err(|error| format!("the {label} endpoint address lookup: {error}"))?;
    if addresses.is_empty() {
        return Err(format!("the {label} endpoint did not resolve"));
    }
    if addresses.iter().any(|address| blocked(address.ip())) {
        return Err(format!(
            "the {label} endpoint resolves to a non-public address"
        ));
    }
    Ok(())
}

fn literal_ip(url: &url::Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => blocked_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => blocked_v4(ip),
            None => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_multicast()
            }
        },
    }
}

fn blocked_v4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
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
    loopback_endpoint(&connection.endpoint().endpoint).then(|| "ene-local".into())
}

fn loopback_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint).is_ok_and(|url| is_loopback(&url))
}

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

/// Reads a mono 16-bit WAV into PCM plus its sample rate. Every chunk is
/// bounds-checked, and a truncated or malformed container is an error, not a
/// panic.
fn parse_wav(bytes: &[u8]) -> HttpResult<(Vec<u8>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("the TTS endpoint did not return a WAV file".into());
    }
    let (mut format, mut data) = (None, None);
    let mut offset = 12usize;
    while let Some(header_end) = offset.checked_add(8) {
        let Some(header) = bytes.get(offset..header_end) else {
            break;
        };
        let size = le32(header, 4) as usize;
        let start = header_end;
        let end = start
            .checked_add(size)
            .ok_or_else(|| "the TTS endpoint returned a malformed WAV chunk".to_owned())?;
        if end > bytes.len() {
            return Err("the TTS endpoint returned a truncated WAV chunk".into());
        }
        match &header[..4] {
            b"fmt " => {
                if size < 16 {
                    return Err("the TTS endpoint returned a malformed WAV format".into());
                }
                format = Some((
                    le16(bytes, start + 2),
                    le32(bytes, start + 4),
                    le16(bytes, start + 14),
                ));
            }
            b"data" => data = Some(&bytes[start..end]),
            _ => {}
        }
        offset = end + (size & 1);
    }
    let (channels, rate, bits) =
        format.ok_or_else(|| "the TTS endpoint returned a WAV without a format".to_owned())?;
    let data = data.ok_or_else(|| "the TTS endpoint returned a WAV without audio".to_owned())?;
    if bits != 16 {
        return Err("the TTS endpoint returned audio that is not 16-bit PCM".into());
    }
    let channels = channels as usize;
    match channels {
        0 => Err("the TTS endpoint returned a WAV without channels".into()),
        1 => Ok((data.to_vec(), rate)),
        channels => {
            let mut mono = Vec::with_capacity(data.len() / channels);
            for frame in data.chunks_exact(2 * channels) {
                let sum: i32 = (0..channels)
                    .map(|channel| i32::from(le16(frame, channel * 2)))
                    .sum();
                let sample =
                    (sum / channels as i32).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
                mono.extend_from_slice(&(sample as i16).to_le_bytes());
            }
            Ok((mono, rate))
        }
    }
}

#[derive(Clone)]
struct Stream {
    subscription: String,
    audio: mpsc::Sender<w::AudioOutputEvent>,
    control: mpsc::UnboundedSender<w::AudioOutputEvent>,
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
    buffer: String,
    total_sentences: u32,
    deadline: Option<Instant>,
}

#[derive(Clone, Default)]
struct Owner {
    client_id: String,
    activation_revision: u64,
    generation_id: u64,
    utterance: Option<Utterance>,
}

#[derive(Default)]
struct State {
    next_generation: u64,
    floor: Option<String>,
    streams: HashMap<String, Stream>,
    owners: HashMap<String, Owner>,
    transcriptions: VecDeque<String>,
}

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
        let (audio, receiver) = mpsc::channel(AUDIO_QUEUE);
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
        let owner = state.owners.entry(companion.to_owned()).or_default();
        owner.client_id = client.to_owned();
        owner.activation_revision = revision;
    }

    fn owner_of(&self, utterance_id: &str) -> Option<(String, Owner)> {
        self.lock().owners.iter().find_map(|(companion, owner)| {
            owner
                .utterance
                .as_ref()
                .is_some_and(|utterance| utterance.id == utterance_id)
                .then(|| (companion.clone(), owner.clone()))
        })
    }

    fn owns(&self, companion: &str) -> bool {
        self.lock().owners.contains_key(companion)
    }

    fn streaming(&self, companion: &str) -> bool {
        self.lock().owners.get(companion).is_some_and(|owner| {
            owner
                .utterance
                .as_ref()
                .is_some_and(|utterance| !utterance.finished)
        })
    }

    fn floor_holder(&self) -> Option<String> {
        self.lock().floor.clone()
    }

    fn begin(&self, companion: &str, source: TtsSource, voice: String) -> bool {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return false;
        }
        self.finish(&mut state, companion, Some(w::FlushReason::Cancelled));
        state.next_generation += 1;
        let generation_id = state.next_generation;
        let Some(owner) = state.owners.get_mut(companion) else {
            return false;
        };
        owner.generation_id = generation_id;
        owner.utterance = Some(Utterance {
            id: uuid::Uuid::new_v4().to_string(),
            generation_id,
            next_sequence: 0,
            chars: 0,
            finished: false,
            voice,
            source,
            buffer: String::new(),
            total_sentences: 0,
            deadline: None,
        });
        if state
            .floor
            .as_deref()
            .is_none_or(|holder| holder == companion)
        {
            state.floor = Some(companion.to_owned());
        }
        true
    }

    fn speak(&self, companion: &str, text: &str) {
        let mut state = self.lock();
        let mut jobs = Vec::new();
        let mut overflow = false;
        {
            let Some(utterance) = state
                .owners
                .get_mut(companion)
                .and_then(|owner| owner.utterance.as_mut())
            else {
                return;
            };
            utterance.buffer.push_str(text);
            for text in split_sentences(&mut utterance.buffer) {
                if utterance.next_sequence >= MAX_SENTENCES {
                    overflow = true;
                    break;
                }
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
        }
        if overflow {
            self.finish(&mut state, companion, Some(w::FlushReason::Cancelled));
            return;
        }
        drop(state);
        for job in jobs {
            self.job(job);
        }
    }

    fn end_turn(&self, companion: &str) {
        let mut state = self.lock();
        if state.floor.as_deref() != Some(companion) {
            return;
        }
        let mut jobs = Vec::new();
        let mut overflow = false;
        let Some(utterance) = state
            .owners
            .get_mut(companion)
            .and_then(|owner| owner.utterance.as_mut())
        else {
            self.finish(&mut state, companion, None);
            return;
        };
        if utterance.finished {
            return;
        }
        if let Some(text) = flush_sentences(&mut utterance.buffer) {
            if utterance.next_sequence >= MAX_SENTENCES {
                overflow = true;
            } else {
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
        }
        if overflow {
            self.finish(&mut state, companion, Some(w::FlushReason::Cancelled));
            return;
        }
        utterance.finished = true;
        let total_sentences = utterance.next_sequence;
        if total_sentences == 0 {
            self.finish(&mut state, companion, None);
            return;
        }
        utterance.total_sentences = total_sentences;
        let speech = Duration::from_secs_f64((f64::from(utterance.chars) * 0.18).min(600.0));
        utterance.deadline = Some(Instant::now() + speech + PLAYBACK_GRACE);
        let utterance_id = utterance.id.clone();
        for job in jobs {
            self.job(job);
        }
        self.job(Job::Finish {
            utterance_id,
            total_sentences,
        });
    }

    fn flush(&self, companion: &str, reason: w::FlushReason) -> Result<()> {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return Err(HostError::not_found(
                "no client owns this companion's audio output",
            ));
        }
        self.finish(&mut state, companion, Some(reason));
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
        self.finish(&mut state, companion, None);
    }

    /// Takes the current utterance, optionally flushes its client, invalidates
    /// the worker and frees the floor.
    fn finish(&self, state: &mut State, companion: &str, reason: Option<w::FlushReason>) {
        let Some(owner) = state.owners.get_mut(companion) else {
            return;
        };
        if let Some(utterance) = owner.utterance.take() {
            if let Some(reason) = reason
                && let Some(stream) = state.streams.get(&owner.client_id)
            {
                let _ =
                    stream
                        .control
                        .send(flush_event(&utterance.id, owner.generation_id, reason));
            }
            self.job(Job::Invalidate {
                utterance_id: utterance.id,
            });
        }
        owner.generation_id = 0;
        if state.floor.as_deref() == Some(companion) {
            state.floor = None;
        }
    }

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

fn flush_sentences(buffer: &mut String) -> Option<String> {
    let text = buffer.trim().to_owned();
    buffer.clear();
    (!text.is_empty()).then_some(text)
}

enum Job {
    Sentence(Sentence),
    Finish {
        utterance_id: String,
        total_sentences: u32,
    },
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

/// Releases synthesized audio in `sequence_no` order; a failed sentence is
/// announced at once so the client never waits for it.
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
        if index >= MAX_SENTENCES as usize {
            emitted.push(skipped_event(
                &self.utterance_id,
                self.generation_id,
                sequence_no,
                "sentence sequence overflow",
            ));
            return emitted;
        }
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        match outcome {
            Ok(pcm) => self.slots[index] = Some(Slot::Audio(pcm)),
            Err(reason) => {
                emitted.push(skipped_event(
                    &self.utterance_id,
                    self.generation_id,
                    sequence_no,
                    reason,
                ));
                self.slots[index] = Some(Slot::Skipped);
            }
        }
        self.drain(&mut emitted);
        self.emit_finished(&mut emitted);
        emitted
    }

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

/// The one synthesis worker. Delivery never awaits a client.
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
                    Some(job) => self.accept(job),
                    None => open = false,
                },
                completion = completed.recv() => {
                    if let Some((utterance_id, sequence_no, outcome)) = completion {
                        self.complete(utterance_id, sequence_no, outcome);
                    }
                }
            }
        }
    }

    fn accept(&mut self, job: Job) {
        match job {
            Job::Sentence(sentence) => {
                let utterance_id = sentence.utterance_id.clone();
                self.work
                    .entry(utterance_id.clone())
                    .or_insert_with(|| Work::new(&sentence));
                let Some(work) = self.work.get_mut(&utterance_id) else {
                    return;
                };
                if work.pending.len() >= MAX_SENTENCES as usize {
                    let events = work
                        .emitter
                        .offer(sentence.sequence_no, Err("synthesis queue overflow".into()));
                    let (companion, generation_id) = (work.companion.clone(), work.generation_id);
                    self.send(&companion, generation_id, events, &utterance_id);
                    return;
                }
                work.pending.push_back(sentence);
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
                self.send(&companion, generation_id, events, &utterance_id);
                self.retire_if_done(&utterance_id);
            }
            Job::Invalidate { utterance_id } => {
                self.work.remove(&utterance_id);
            }
        }
    }

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

    fn complete(&mut self, utterance_id: String, sequence_no: u32, outcome: HttpResult<Vec<u8>>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let Some((companion, generation_id, events)) = self.events(&utterance_id, |work| {
            work.in_flight = work.in_flight.saturating_sub(1);
            work.emitter.offer(sequence_no, outcome)
        }) else {
            return;
        };
        self.send(&companion, generation_id, events, &utterance_id);
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

    fn send(
        &mut self,
        companion: &str,
        generation_id: u64,
        events: Vec<w::AudioOutputEvent>,
        utterance_id: &str,
    ) {
        for event in events {
            match deliver(&self.shared, companion, generation_id, event) {
                Dispatch::Sent => {}
                Dispatch::Stale => {
                    self.work.remove(utterance_id);
                    return;
                }
                Dispatch::Full | Dispatch::Closed => {
                    fail_owner(&self.shared, companion);
                    self.work.retain(|_, work| work.companion != companion);
                    tracing::debug!(%companion, "the audio client stalled; its utterance was cancelled");
                    return;
                }
            }
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

enum Dispatch {
    Sent,
    Stale,
    Full,
    Closed,
}

/// Sends `event` to the owning client when its generation is current. This
/// never waits; a full queue tells the caller to flush that owner alone.
fn deliver(
    shared: &Mutex<State>,
    companion: &str,
    generation_id: u64,
    event: w::AudioOutputEvent,
) -> Dispatch {
    let state = lock(shared);
    let Some(owner) = state.owners.get(companion) else {
        return Dispatch::Stale;
    };
    if owner
        .utterance
        .as_ref()
        .is_none_or(|utterance| utterance.generation_id != generation_id)
    {
        return Dispatch::Stale;
    }
    let Some(stream) = state.streams.get(&owner.client_id) else {
        return Dispatch::Stale;
    };
    match stream.audio.try_send(event) {
        Ok(()) => Dispatch::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => Dispatch::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => Dispatch::Closed,
    }
}

/// Cancels one stalled owner: it alone is flushed and loses ownership and the
/// floor, so other companions keep speaking.
fn fail_owner(shared: &Mutex<State>, companion: &str) -> bool {
    let mut state = lock(shared);
    let Some(owner) = state.owners.remove(companion) else {
        return false;
    };
    if let Some(utterance) = &owner.utterance
        && let Some(stream) = state.streams.get(&owner.client_id)
    {
        let _ = stream.control.send(flush_event(
            &utterance.id,
            owner.generation_id,
            w::FlushReason::Cancelled,
        ));
    }
    if state.floor.as_deref() == Some(companion) {
        state.floor = None;
    }
    true
}

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

fn skipped_event(
    utterance_id: &str,
    generation_id: u64,
    sequence_no: u32,
    reason: impl Into<String>,
) -> w::AudioOutputEvent {
    audio_event(w::SentenceSkipped {
        utterance_id: utterance_id.to_owned(),
        generation_id,
        sequence_no,
        reason: reason.into(),
        ..Default::default()
    })
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
    pub(crate) fn accepts(&self, chunk: &w::AudioInputChunk) -> bool {
        (chunk.client_id.is_empty() || chunk.client_id == self.client_id)
            && (chunk.activation_revision == 0
                || chunk.activation_revision == self.activation_revision)
    }
}

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

pub(crate) struct AudioSubscription {
    pub(crate) subscription_id: String,
    pub(crate) receiver: mpsc::Receiver<w::AudioOutputEvent>,
    pub(crate) control: mpsc::UnboundedReceiver<w::AudioOutputEvent>,
}

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

pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.unregister(client, subscription);
    }
}

pub(crate) fn report_playback(
    world: &mut World,
    report: w::PlaybackStatusReport,
) -> Result<w::ReportPlaybackStatusResponse> {
    let response = |acknowledged| w::ReportPlaybackStatusResponse {
        acknowledged,
        ..Default::default()
    };
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(response(false));
    };
    db::validate_id(&report.client_id)?;
    if store.client(&report.client_id)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Ok(response(false));
    };
    let Some((companion, owner)) = voice.owner_of(&report.utterance_id) else {
        return Ok(response(false));
    };
    if owner.client_id != report.client_id
        || owner.activation_revision != report.activation_revision
        || owner.generation_id != report.generation_id
    {
        return Ok(response(false));
    }
    let current = store
        .activation(&companion)?
        .map(|activation| activation.revision)
        .unwrap_or(0);
    if current != owner.activation_revision {
        return Ok(response(false));
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
        return Ok(response(false));
    }
    let total_sentences = owner
        .utterance
        .as_ref()
        .map(|utterance| utterance.total_sentences);
    voice.release(&companion);
    if completed {
        broadcast_speech_completed(world, &companion, &report, total_sentences);
    }
    Ok(response(true))
}

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

pub(crate) fn accept_transcription(
    world: &mut World,
    transcription_id: &str,
    text: &str,
    binding: &InputBinding,
) -> Result<bool> {
    let text = text.trim();
    if text.is_empty() || !is_still_active(world, binding) {
        return Ok(false);
    }
    if let Some(voice) = world.get_resource::<Voice>()
        && !voice.accept_transcription(transcription_id)
    {
        return Ok(false);
    }
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(false);
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
                    "client_id": binding.client_id,
                }),
            ));
        }
        world.trigger(
            Deliver::new(agent, text.to_owned(), DeliveryMode::Steer).with_author(author.clone()),
        );
    }
    Ok(true)
}

pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    if !voice.owns(companion) {
        return;
    }
    if !voice.streaming(companion) {
        let Ok(source) = tts_source(world) else {
            return;
        };
        if let Some(holder) = voice.floor_holder()
            && holder != companion
        {
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

pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.end_turn(companion);
    }
}

pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::not_found(
            "no client owns this companion's audio output",
        ));
    };
    voice.flush(companion, reason)
}

pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str, revision: u64) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        voice.client_moved(companion, client, revision);
    }
}

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
            .filter(|(_, owner)| {
                owner
                    .utterance
                    .as_ref()
                    .and_then(|utterance| utterance.deadline)
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(companion, _)| companion.clone())
            .collect()
    };
    for companion in expired {
        tracing::debug!(%companion, "playback did not report; releasing the speech floor");
        voice.release(&companion);
    }
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        let (voice, receiver) = Voice::new();
        spawn_worker(voice.shared(), receiver);
        app.insert_resource(voice)
            .add_systems(Update, expire_playback);
    }
}

#[cfg(test)]
#[derive(Resource, Default)]
struct TestEngines {
    tts: Option<Arc<dyn TtsEngine>>,
    stt: Option<Arc<dyn SttEngine>>,
}

#[cfg(test)]
mod tests;
