//! The host-side voice pipeline: speech recognition input, sentence-level
//! speech synthesis output, playback ownership and the speech floor
//! (docs/design/voice-avatar.md §4–§5, docs/api.md §6–§7).
//!
//! The pipeline has two sides on purpose. The host thread only keeps state: it
//! splits the agent's streamed text into sentences, assigns generations and
//! `sequence_no`s, records the audio-owning client and the floor, and enqueues
//! synthesis jobs. A dedicated worker thread owns a current-thread Tokio
//! runtime, runs the HTTP TTS requests (at most three sentences in flight per
//! utterance) and releases the results to the client in `sequence_no` order.
//! Speech recognition runs inside the Connect layer, where the bidirectional
//! request already lives.
//!
//! A host with no TTS/STT connection keeps the text pipeline only: the RPC
//! edge reports the capability as `unimplemented`, and no state is created.
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
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
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

// ---------------------------------------------------------------------------
// Engines: the one seam between the host and a provider.
// ---------------------------------------------------------------------------

/// One sentence handed to speech synthesis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TtsRequest {
    pub(crate) text: String,
    /// The companion's `voice_identifier`, or the connection's default.
    pub(crate) voice: String,
}

/// Synthesized audio, already mono, before the wire conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TtsAudio {
    pub(crate) pcm: Vec<u8>,
    pub(crate) sample_rate: u32,
}

/// The speech-synthesis engine seam. Implemented by the HTTP adapter in
/// production and by a fake engine in the unit tests.
pub(crate) trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        request: TtsRequest,
    ) -> BoxFuture<'a, std::result::Result<TtsAudio, String>>;
}

/// One utterance's audio handed to speech recognition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SttRequest {
    pub(crate) pcm: Vec<u8>,
    pub(crate) sample_rate: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SttResult {
    pub(crate) text: String,
}

/// The speech-recognition engine seam.
pub(crate) trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        request: SttRequest,
    ) -> BoxFuture<'a, std::result::Result<SttResult, String>>;
}

/// How the HTTP TTS adapter talks to its endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TtsProtocol {
    /// `POST {base}{path}` `/v1/audio/speech`, `response_format: pcm`.
    OpenAi,
    /// VOICEVOX: `audio_query` then `synthesis`.
    Voicevox,
}

/// How the HTTP STT adapter talks to its endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SttProtocol {
    /// OpenAI-compatible multipart `/v1/audio/transcriptions`.
    OpenAi,
    /// A plain body of WAV bytes whose response is the transcript text.
    Raw,
}

/// Everything the HTTP TTS adapter needs, resolved from a `TtsConnection`.
#[derive(Clone, Debug)]
pub(crate) struct TtsSetup {
    pub(crate) connection_id: String,
    pub(crate) revision: u64,
    pub(crate) base: String,
    pub(crate) protocol: TtsProtocol,
    pub(crate) model: String,
    pub(crate) voice: String,
    pub(crate) path: String,
    pub(crate) query_path: String,
    pub(crate) synthesis_path: String,
    pub(crate) speaker: u32,
    pub(crate) speed: Option<f32>,
    pub(crate) secret: Option<String>,
    pub(crate) timeout: Duration,
}

/// Everything the HTTP STT adapter needs, resolved from an `SttConnection`.
#[derive(Clone, Debug)]
pub(crate) struct SttSetup {
    pub(crate) base: String,
    pub(crate) protocol: SttProtocol,
    pub(crate) path: String,
    pub(crate) model: String,
    pub(crate) language: Option<String>,
    pub(crate) secret: Option<String>,
    pub(crate) timeout: Duration,
}

/// Where the worker gets its engine from.
#[derive(Clone)]
pub(crate) enum TtsSource {
    Connection(Arc<TtsSetup>),
    #[cfg(test)]
    Fixed(Arc<dyn TtsEngine>),
}

impl TtsSource {
    fn engine_key(&self) -> Option<String> {
        match self {
            Self::Connection(setup) => Some(format!("{}:{}", setup.connection_id, setup.revision)),
            #[cfg(test)]
            Self::Fixed(_) => None,
        }
    }

    fn engine(&self) -> Arc<dyn TtsEngine> {
        match self {
            Self::Connection(setup) => {
                let engine: Arc<dyn TtsEngine> = Arc::new(HttpTtsEngine::new(Arc::clone(setup)));
                engine
            }
            #[cfg(test)]
            Self::Fixed(engine) => Arc::clone(engine),
        }
    }
}

/// Where the RPC layer gets its recognition engine from.
#[derive(Clone)]
pub(crate) enum SttSource {
    Connection(Arc<SttSetup>),
    #[cfg(test)]
    Fixed(Arc<dyn SttEngine>),
}

impl SttSource {
    pub(crate) fn engine(&self) -> Arc<dyn SttEngine> {
        match self {
            Self::Connection(setup) => {
                let engine: Arc<dyn SttEngine> = Arc::new(HttpSttEngine::new(Arc::clone(setup)));
                engine
            }
            #[cfg(test)]
            Self::Fixed(engine) => Arc::clone(engine),
        }
    }
}

/// The HTTP text-to-speech adapter. The OpenAI protocol asks for raw
/// `pcm` (24 kHz, 16-bit, mono, little-endian); the VOICEVOX protocol takes a
/// WAV back and converts it to the wire standard.
pub(crate) struct HttpTtsEngine {
    client: Option<reqwest::Client>,
    failure: Option<String>,
    setup: Arc<TtsSetup>,
}

impl HttpTtsEngine {
    fn new(setup: Arc<TtsSetup>) -> Self {
        let built = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(setup.timeout)
            .build();
        match built {
            Ok(client) => Self {
                client: Some(client),
                failure: None,
                setup,
            },
            Err(error) => Self {
                client: None,
                failure: Some(format!("http client: {error}")),
                setup,
            },
        }
    }

    fn client(&self) -> std::result::Result<&reqwest::Client, String> {
        self.client
            .as_ref()
            .ok_or_else(|| self.failure.clone().unwrap_or_else(|| "http client".into()))
    }

    async fn openai(&self, request: TtsRequest) -> std::result::Result<TtsAudio, String> {
        let client = self.client()?;
        let mut body = serde_json::json!({
            "model": self.setup.model,
            "voice": if request.voice.is_empty() { &self.setup.voice } else { &request.voice },
            "input": request.text,
            "response_format": "pcm",
        });
        if let Some(speed) = self.setup.speed {
            body["speed"] = serde_json::json!(speed);
        }
        let url = format!("{}{}", self.setup.base, self.setup.path);
        let mut call = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(secret) = &self.setup.secret {
            call = call.bearer_auth(secret);
        }
        let response = call
            .send()
            .await
            .map_err(|error| format!("the TTS endpoint is unreachable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the TTS endpoint answered {}",
                response.status().as_u16()
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("the TTS endpoint body: {error}"))?;
        Ok(TtsAudio {
            pcm: bytes.to_vec(),
            sample_rate: SAMPLE_RATE,
        })
    }

    async fn voicevox(&self, request: TtsRequest) -> std::result::Result<TtsAudio, String> {
        let client = self.client()?;
        let encoded: String =
            url::form_urlencoded::byte_serialize(request.text.as_bytes()).collect();
        let query_url = format!(
            "{}{}?text={}&speaker={}",
            self.setup.base, self.setup.query_path, encoded, self.setup.speaker
        );
        let mut query_call = client.post(query_url);
        if let Some(secret) = &self.setup.secret {
            query_call = query_call.bearer_auth(secret);
        }
        let query = query_call
            .send()
            .await
            .map_err(|error| format!("the TTS endpoint is unreachable: {error}"))?;
        if !query.status().is_success() {
            return Err(format!(
                "the TTS endpoint answered {}",
                query.status().as_u16()
            ));
        }
        let text = query
            .text()
            .await
            .map_err(|error| format!("the TTS endpoint body: {error}"))?;
        let mut body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| "the TTS endpoint answered a non-JSON audio query".to_owned())?;
        if let Some(speed) = self.setup.speed {
            body["speedScale"] = serde_json::json!(speed);
        }
        let synthesis_url = format!(
            "{}{}?speaker={}",
            self.setup.base, self.setup.synthesis_path, self.setup.speaker
        );
        let mut synthesis_call = client
            .post(synthesis_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(secret) = &self.setup.secret {
            synthesis_call = synthesis_call.bearer_auth(secret);
        }
        let response = synthesis_call
            .send()
            .await
            .map_err(|error| format!("the TTS endpoint is unreachable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the TTS endpoint answered {}",
                response.status().as_u16()
            ));
        }
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
        request: TtsRequest,
    ) -> BoxFuture<'a, std::result::Result<TtsAudio, String>> {
        Box::pin(async move {
            match self.setup.protocol {
                TtsProtocol::OpenAi => self.openai(request).await,
                TtsProtocol::Voicevox => self.voicevox(request).await,
            }
        })
    }
}

/// The HTTP speech-recognition adapter.
pub(crate) struct HttpSttEngine {
    client: Option<reqwest::Client>,
    failure: Option<String>,
    setup: Arc<SttSetup>,
}

impl HttpSttEngine {
    fn new(setup: Arc<SttSetup>) -> Self {
        let built = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(setup.timeout)
            .build();
        match built {
            Ok(client) => Self {
                client: Some(client),
                failure: None,
                setup,
            },
            Err(error) => Self {
                client: None,
                failure: Some(format!("http client: {error}")),
                setup,
            },
        }
    }

    async fn openai(
        &self,
        client: &reqwest::Client,
        wav: &[u8],
    ) -> std::result::Result<SttResult, String> {
        let boundary = format!("----enevoice{}", uuid::Uuid::new_v4().simple());
        let body = multipart_body(&boundary, &self.setup, wav);
        let url = format!("{}{}", self.setup.base, self.setup.path);
        let mut call = client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(secret) = &self.setup.secret {
            call = call.bearer_auth(secret);
        }
        let response = call
            .send()
            .await
            .map_err(|error| format!("the STT endpoint is unreachable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the STT endpoint answered {}",
                response.status().as_u16()
            ));
        }
        let text = response
            .text()
            .await
            .map_err(|error| format!("the STT endpoint body: {error}"))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| "the STT endpoint answered a non-JSON transcript".to_owned())?;
        let transcript = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        Ok(SttResult { text: transcript })
    }

    async fn raw(
        &self,
        client: &reqwest::Client,
        wav: &[u8],
    ) -> std::result::Result<SttResult, String> {
        let url = format!("{}{}", self.setup.base, self.setup.path);
        let mut call = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(wav.to_vec());
        if let Some(secret) = &self.setup.secret {
            call = call.bearer_auth(secret);
        }
        let response = call
            .send()
            .await
            .map_err(|error| format!("the STT endpoint is unreachable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the STT endpoint answered {}",
                response.status().as_u16()
            ));
        }
        let text = response
            .text()
            .await
            .map_err(|error| format!("the STT endpoint body: {error}"))?;
        Ok(SttResult {
            text: text.trim().to_owned(),
        })
    }
}

impl SttEngine for HttpSttEngine {
    fn transcribe<'a>(
        &'a self,
        request: SttRequest,
    ) -> BoxFuture<'a, std::result::Result<SttResult, String>> {
        Box::pin(async move {
            let Some(client) = self.client.as_ref() else {
                return Err(self.failure.clone().unwrap_or_else(|| "http client".into()));
            };
            let wav = wav_wrap(&request.pcm, request.sample_rate);
            match self.setup.protocol {
                SttProtocol::OpenAi => self.openai(client, &wav).await,
                SttProtocol::Raw => self.raw(client, &wav).await,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Connection resolution.
// ---------------------------------------------------------------------------

/// Resolves the TTS engine source, or why the capability is unavailable.
pub(crate) fn tts_source(world: &World) -> std::result::Result<TtsSource, SetupError> {
    #[cfg(test)]
    {
        if let Some(test) = world.get_resource::<TestEngines>()
            && let Some(engine) = &test.tts
        {
            return Ok(TtsSource::Fixed(Arc::clone(engine)));
        }
    }
    let connection = voice_connection(world, ConnectionKind::Tts)?;
    let credentials = world.get_resource::<Credentials>();
    let setup = build_tts_setup(connection, credentials)?;
    Ok(TtsSource::Connection(Arc::new(setup)))
}

/// Resolves the STT engine source, or why the capability is unavailable.
pub(crate) fn stt_source(world: &World) -> std::result::Result<SttSource, SetupError> {
    #[cfg(test)]
    {
        if let Some(test) = world.get_resource::<TestEngines>()
            && let Some(engine) = &test.stt
        {
            return Ok(SttSource::Fixed(Arc::clone(engine)));
        }
    }
    let connection = voice_connection(world, ConnectionKind::Stt)?;
    let credentials = world.get_resource::<Credentials>();
    let setup = build_stt_setup(connection, credentials)?;
    Ok(SttSource::Connection(Arc::new(setup)))
}

/// Whether a connection of `kind` exists and is enabled. A host without one
/// reports the voice capability as unimplemented.
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

fn build_tts_setup(
    connection: Connection,
    credentials: Option<&Credentials>,
) -> std::result::Result<TtsSetup, SetupError> {
    let endpoint = connection.endpoint().clone();
    let base = base_url(&endpoint, "TTS")?;
    let secret = credentials.and_then(|credentials| secret_for(&connection, credentials));
    if secret.is_none() && !loopback_endpoint(&base) {
        return Err(SetupError::Broken(
            "no credential is registered for the TTS connection".into(),
        ));
    }
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
    let voice = option_str(&options, "voice")
        .map(ToOwned::to_owned)
        .or(default_voice)
        .unwrap_or_else(|| "alloy".into());
    let model = option_str(&options, "model")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "tts-1".into());
    let path = path_option(&options, "path", "/audio/speech");
    let query_path = path_option(&options, "audio_query_path", "/audio_query");
    let synthesis_path = path_option(&options, "synthesis_path", "/synthesis");
    let speaker = option_u64(&options, "speaker").unwrap_or(1) as u32;
    let speed = option_f64(&options, "speed").map(|speed| speed as f32);
    let timeout = option_u64(&options, "timeout_ms")
        .unwrap_or(20_000)
        .clamp(1_000, 300_000);
    Ok(TtsSetup {
        connection_id: connection.id,
        revision: connection.revision,
        base,
        protocol,
        model,
        voice,
        path,
        query_path,
        synthesis_path,
        speaker,
        speed,
        secret,
        timeout: Duration::from_millis(timeout),
    })
}

fn build_stt_setup(
    connection: Connection,
    credentials: Option<&Credentials>,
) -> std::result::Result<SttSetup, SetupError> {
    let endpoint = connection.endpoint().clone();
    let base = base_url(&endpoint, "STT")?;
    let secret = credentials.and_then(|credentials| secret_for(&connection, credentials));
    if secret.is_none() && !loopback_endpoint(&base) {
        return Err(SetupError::Broken(
            "no credential is registered for the STT connection".into(),
        ));
    }
    let options = option_map(&endpoint)?;
    let protocol_name = option_str(&options, "protocol").unwrap_or("openai");
    let protocol = match protocol_name {
        "openai" => SttProtocol::OpenAi,
        "raw" => SttProtocol::Raw,
        other => {
            return Err(SetupError::Unsupported(format!(
                "the STT adapter protocol `{other}` is not supported"
            )));
        }
    };
    let hints = match &connection.config {
        ConnectionConfig::Stt(config) => config.language_hints.clone(),
        _ => Vec::new(),
    };
    let language = option_str(&options, "language")
        .map(ToOwned::to_owned)
        .or_else(|| hints.first().cloned());
    let model = option_str(&options, "model")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "whisper-1".into());
    let path = path_option(&options, "path", "/audio/transcriptions");
    let timeout = option_u64(&options, "timeout_ms")
        .unwrap_or(30_000)
        .clamp(1_000, 300_000);
    Ok(SttSetup {
        base,
        protocol,
        path,
        model,
        language,
        secret,
        timeout: Duration::from_millis(timeout),
    })
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

// ---------------------------------------------------------------------------
// Audio conversion: the wire standard is 24 kHz / 16-bit / mono.
// ---------------------------------------------------------------------------

/// The TTS audio converted to the wire standard.
fn wire_pcm(audio: TtsAudio) -> std::result::Result<Vec<u8>, String> {
    if !audio.pcm.len().is_multiple_of(2) {
        return Err("the TTS engine returned an odd number of PCM bytes".into());
    }
    if audio.sample_rate == 0 {
        return Err("the TTS engine returned audio without a sample rate".into());
    }
    if audio.sample_rate == SAMPLE_RATE {
        return Ok(audio.pcm);
    }
    Ok(resample_mono(&audio.pcm, audio.sample_rate, SAMPLE_RATE))
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
        let fraction = position - left as f64;
        let first = f64::from(samples.get(left).copied().unwrap_or(0));
        let second = f64::from(samples.get(left + 1).copied().unwrap_or(first as i16));
        let value = first + (second - first) * fraction;
        let sample = value
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// A minimal PCM WAV container for the STT endpoint.
pub(crate) fn wav_wrap(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
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

/// The multipart body a transcription endpoint expects. The `multipart`
/// feature of reqwest is not part of this crate's dependency set, so the body
/// is assembled here.
fn multipart_body(boundary: &str, setup: &SttSetup, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    for (name, value) in [
        ("model", Some(setup.model.as_str())),
        ("language", setup.language.as_deref()),
        ("response_format", Some("json")),
    ] {
        let Some(value) = value else {
            continue;
        };
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Reads a mono 16-bit WAV into PCM plus its sample rate.
fn parse_wav(bytes: &[u8]) -> std::result::Result<TtsAudio, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("the TTS endpoint did not return a WAV file".into());
    }
    let mut offset = 12usize;
    let mut format: Option<(u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let start = offset + 8;
        let end = start.saturating_add(size).min(bytes.len());
        if id == b"fmt " && size >= 16 {
            let channels = u16::from_le_bytes([bytes[start + 2], bytes[start + 3]]);
            let rate = u32::from_le_bytes([
                bytes[start + 4],
                bytes[start + 5],
                bytes[start + 6],
                bytes[start + 7],
            ]);
            let bits = u16::from_le_bytes([bytes[start + 14], bytes[start + 15]]);
            format = Some((channels, rate, bits));
        } else if id == b"data" {
            data = Some(&bytes[start..end]);
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
    let pcm = if channels == 1 {
        data.to_vec()
    } else {
        let mut mono = Vec::with_capacity(data.len() / channels);
        for frame in data.chunks_exact(2 * channels) {
            let mut sum = 0i32;
            for channel in 0..channels {
                sum += i32::from(i16::from_le_bytes([
                    frame[channel * 2],
                    frame[channel * 2 + 1],
                ]));
            }
            let sample = (sum / channels as i32).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            mono.extend_from_slice(&(sample as i16).to_le_bytes());
        }
        mono
    };
    Ok(TtsAudio {
        pcm,
        sample_rate: rate,
    })
}

// ---------------------------------------------------------------------------
// Shared synthesis side: the hub, the resources and the worker.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct HubState {
    streams: HashMap<String, Stream>,
    owners: HashMap<String, Owner>,
}

#[derive(Clone)]
struct Stream {
    subscription: String,
    /// Audio and the end-of-utterance markers; the worker waits for room.
    sender: mpsc::Sender<w::AudioOutputEvent>,
    /// Flushes and other control markers; never dropped for lack of room.
    control: mpsc::UnboundedSender<w::AudioOutputEvent>,
}

#[derive(Clone)]
struct Owner {
    client_id: String,
    activation_revision: u64,
    utterance_id: String,
    generation_id: u64,
}

/// The live `SubscribeAudioOutput` streams and the generation gate, shared by
/// the host thread, the synthesis worker and the Connect layer. Events for a
/// generation the host already invalidated are dropped here.
#[derive(Clone, Default, Resource)]
pub(crate) struct VoiceHub {
    inner: Arc<Mutex<HubState>>,
}

impl VoiceHub {
    /// Registers one client's audio stream. A newer stream supersedes the old.
    fn register(
        &self,
        client: &str,
    ) -> (
        String,
        mpsc::Receiver<w::AudioOutputEvent>,
        mpsc::UnboundedReceiver<w::AudioOutputEvent>,
    ) {
        let (sender, receiver) = mpsc::channel(64);
        let (control, control_receiver) = mpsc::unbounded_channel();
        let subscription = uuid::Uuid::new_v4().to_string();
        let mut state = self.lock();
        state.streams.insert(
            client.to_owned(),
            Stream {
                subscription: subscription.clone(),
                sender,
                control,
            },
        );
        (subscription, receiver, control_receiver)
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

    /// Records that `client` owns `companion`'s playback.
    fn publish(
        &self,
        companion: &str,
        client: &str,
        revision: u64,
        utterance_id: Option<&str>,
        generation_id: u64,
    ) {
        let mut state = self.lock();
        let owner = state
            .owners
            .entry(companion.to_owned())
            .or_insert_with(|| Owner {
                client_id: client.to_owned(),
                activation_revision: revision,
                utterance_id: String::new(),
                generation_id,
            });
        owner.client_id = client.to_owned();
        owner.activation_revision = revision;
        if let Some(utterance_id) = utterance_id {
            owner.utterance_id = utterance_id.to_owned();
            owner.generation_id = generation_id;
        }
    }

    /// Keeps the client but stops delivering until the next utterance.
    fn retire(&self, companion: &str) {
        let mut state = self.lock();
        if let Some(owner) = state.owners.get_mut(companion) {
            owner.utterance_id.clear();
            owner.generation_id = 0;
        }
    }

    fn remove(&self, companion: &str) {
        self.lock().owners.remove(companion);
    }

    fn send_to(&self, client: &str, event: w::AudioOutputEvent) -> bool {
        let sender = self
            .lock()
            .streams
            .get(client)
            .map(|stream| stream.control.clone());
        sender.is_some_and(|sender| sender.send(event).is_ok())
    }

    /// Sends `event` to the companion's audio-owning client when its generation
    /// is still current. The send waits for room: dropping an audio chunk would
    /// leave the client waiting for a `sequence_no` that never closes.
    async fn deliver(
        &self,
        companion: &str,
        generation_id: u64,
        event: w::AudioOutputEvent,
    ) -> bool {
        let sender = {
            let state = self.lock();
            let Some(owner) = state.owners.get(companion) else {
                return false;
            };
            if owner.utterance_id.is_empty() || owner.generation_id != generation_id {
                return false;
            }
            let Some(stream) = state.streams.get(&owner.client_id) else {
                return false;
            };
            stream.sender.clone()
        };
        sender.send(event).await.is_ok()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// One companion's current generation. A new utterance or a flush moves it, so
/// delayed synthesis of the old generation is discarded.
#[derive(Resource, Default)]
pub(crate) struct Generations {
    next: u64,
    current: HashMap<String, u64>,
}

impl Generations {
    /// Allocates the next generation for `companion`.
    fn allocate(&mut self, companion: &str) -> u64 {
        self.next += 1;
        self.current.insert(companion.to_owned(), self.next);
        self.next
    }

    /// Invalidates the current generation and returns the replacement.
    fn invalidate(&mut self, companion: &str) -> u64 {
        self.allocate(companion)
    }

    fn current(&self, companion: &str) -> u64 {
        self.current.get(companion).copied().unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UtteranceState {
    Streaming,
    Finished,
}

/// One companion's in-progress utterance.
#[derive(Clone)]
pub(crate) struct ActiveUtterance {
    pub(crate) id: String,
    pub(crate) generation_id: u64,
    pub(crate) next_sequence: u32,
    pub(crate) chars: u32,
    state: UtteranceState,
    voice: String,
    source: TtsSource,
}

#[derive(Resource, Default)]
pub(crate) struct ActiveUtterances {
    by_companion: HashMap<String, ActiveUtterance>,
}

impl ActiveUtterances {
    fn begin(&mut self, companion: &str, utterance: ActiveUtterance) {
        self.by_companion.insert(companion.to_owned(), utterance);
    }

    fn streaming(&self, companion: &str) -> Option<&ActiveUtterance> {
        self.by_companion
            .get(companion)
            .filter(|utterance| utterance.state == UtteranceState::Streaming)
    }

    fn get(&self, companion: &str) -> Option<&ActiveUtterance> {
        self.by_companion.get(companion)
    }

    fn get_mut(&mut self, companion: &str) -> Option<&mut ActiveUtterance> {
        self.by_companion.get_mut(companion)
    }

    fn remove(&mut self, companion: &str) {
        self.by_companion.remove(companion);
    }
}

/// The text that has arrived but has not reached a sentence boundary yet.
#[derive(Resource, Default)]
pub(crate) struct SentenceBuffers {
    by_companion: HashMap<String, String>,
}

impl SentenceBuffers {
    fn entry(&mut self, companion: &str) -> &mut String {
        self.by_companion.entry(companion.to_owned()).or_default()
    }

    fn clear(&mut self, companion: &str) {
        self.by_companion.remove(companion);
    }
}

/// The speech floor: the companion that began streaming text first speaks.
#[derive(Resource, Default)]
pub(crate) struct Floors {
    holder: Option<String>,
}

impl Floors {
    fn holder(&self) -> Option<&str> {
        self.holder.as_deref()
    }

    /// Takes the floor for `companion`; `false` when a peer holds it.
    fn acquire(&mut self, companion: &str) -> bool {
        match &self.holder {
            Some(holder) => holder == companion,
            None => {
                self.holder = Some(companion.to_owned());
                true
            }
        }
    }

    fn release(&mut self, companion: &str) {
        if self.holder.as_deref() == Some(companion) {
            self.holder = None;
        }
    }
}

/// The client that owns one companion's audio output, and the utterance it is
/// playing.
#[derive(Clone, Debug)]
pub(crate) struct PlaybackOwnership {
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
    pub(crate) utterance_id: Option<String>,
    pub(crate) generation_id: u64,
    pub(crate) total_sentences: Option<u32>,
    pub(crate) deadline: Option<Instant>,
}

/// Per-client playback ownership. The client is kept across utterances; the
/// utterance is only set while one is being sent.
#[derive(Resource, Default)]
pub(crate) struct Playback {
    by_companion: HashMap<String, PlaybackOwnership>,
}

impl Playback {
    fn owner(&self, companion: &str) -> Option<&PlaybackOwnership> {
        self.by_companion.get(companion)
    }

    fn publish(
        &mut self,
        companion: &str,
        client: &str,
        revision: u64,
        utterance_id: Option<&str>,
        generation_id: u64,
    ) {
        let owner = self
            .by_companion
            .entry(companion.to_owned())
            .or_insert_with(|| PlaybackOwnership {
                client_id: client.to_owned(),
                activation_revision: revision,
                utterance_id: None,
                generation_id,
                total_sentences: None,
                deadline: None,
            });
        owner.client_id = client.to_owned();
        owner.activation_revision = revision;
        if let Some(utterance_id) = utterance_id {
            owner.utterance_id = Some(utterance_id.to_owned());
            owner.generation_id = generation_id;
            owner.total_sentences = None;
            owner.deadline = None;
        }
    }

    fn begin(&mut self, companion: &str, utterance_id: &str, generation_id: u64) {
        if let Some(owner) = self.by_companion.get_mut(companion) {
            owner.utterance_id = Some(utterance_id.to_owned());
            owner.generation_id = generation_id;
            owner.total_sentences = None;
            owner.deadline = None;
        }
    }

    fn finish(&mut self, companion: &str, total_sentences: u32, deadline: Instant) {
        if let Some(owner) = self.by_companion.get_mut(companion) {
            owner.total_sentences = Some(total_sentences);
            owner.deadline = Some(deadline);
        }
    }

    /// Drops the utterance but keeps the client that owns playback.
    fn take_utterance(&mut self, companion: &str) -> Option<(String, u64)> {
        let owner = self.by_companion.get_mut(companion)?;
        let utterance_id = owner.utterance_id.take()?;
        owner.total_sentences = None;
        owner.deadline = None;
        Some((utterance_id, owner.generation_id))
    }

    fn companion_of_utterance(&self, utterance_id: &str) -> Option<String> {
        self.by_companion
            .iter()
            .find(|(_, owner)| owner.utterance_id.as_deref() == Some(utterance_id))
            .map(|(companion, _)| companion.clone())
    }

    fn remove(&mut self, companion: &str) {
        self.by_companion.remove(companion);
    }
}

/// The synthesis queue shared by the host thread and the worker.
#[derive(Resource)]
pub(crate) struct VoiceQueue {
    jobs: mpsc::UnboundedSender<SynthesisJob>,
}

impl VoiceQueue {
    fn channel() -> (Self, mpsc::UnboundedReceiver<SynthesisJob>) {
        let (jobs, receiver) = mpsc::unbounded_channel();
        (Self { jobs }, receiver)
    }

    fn send(&self, job: SynthesisJob) {
        if self.jobs.send(job).is_err() {
            tracing::debug!("the voice worker is gone; the sentence is dropped");
        }
    }
}

/// Transcription ids that already reached the companions.
#[derive(Resource, Default)]
pub(crate) struct Transcriptions {
    recent: VecDeque<String>,
}

impl Transcriptions {
    /// `false` when this transcription was already delivered.
    fn accept(&mut self, transcription_id: &str) -> bool {
        if transcription_id.is_empty() {
            return true;
        }
        if self.recent.iter().any(|known| known == transcription_id) {
            return false;
        }
        self.recent.push_back(transcription_id.to_owned());
        if self.recent.len() > TRANSCRIPTION_MEMORY {
            self.recent.pop_front();
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Sentence splitting.
// ---------------------------------------------------------------------------

/// The design's sentence terminators (docs/design/voice-avatar.md §4.1).
fn is_terminator(character: char) -> bool {
    matches!(character, '。' | '！' | '？' | '\n')
}

/// Pulls every complete sentence out of `buffer`, leaving the unterminated
/// remainder. A very long unpunctuated run is cut so speech still starts.
fn split_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    for (index, character) in buffer.char_indices() {
        if !is_terminator(character) {
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

// ---------------------------------------------------------------------------
// Synthesis: the worker, the ordering gate and the envelope.
// ---------------------------------------------------------------------------

/// One unit of TTS work. The worker runs at most
/// `MAX_SYNTHESIS_SENTENCES` per utterance concurrently.
pub(crate) enum SynthesisJob {
    Sentence {
        companion: String,
        utterance_id: String,
        generation_id: u64,
        sequence_no: u32,
        text: String,
        source: TtsSource,
        voice: String,
    },
    /// No more sentences will arrive for this utterance.
    Finish {
        companion: String,
        utterance_id: String,
        generation_id: u64,
        total_sentences: u32,
    },
    /// The host invalidated this generation; forget everything about it.
    Invalidate {
        companion: String,
        utterance_id: String,
        generation_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SynthesisOutcome {
    Audio(Vec<u8>),
    Failed(String),
}

/// What the ordering gate releases to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Emitted {
    Chunk {
        sequence_no: u32,
        pcm: Vec<u8>,
        is_final_chunk: bool,
    },
    Skipped {
        sequence_no: u32,
        reason: String,
    },
    Finished {
        total_sentences: u32,
    },
}

/// Releases synthesized audio in `sequence_no` order. A sentence that finishes
/// first waits for the earlier ones; a failed sentence is announced at once so
/// the client never waits for it. A chunk is final when it closes its sentence.
#[derive(Debug, Default)]
pub(crate) struct Emitter {
    next: u32,
    finished: Option<u32>,
    sent_finished: bool,
    audio: BTreeMap<u32, Vec<u8>>,
    done: BTreeSet<u32>,
}

impl Emitter {
    /// Records one sentence's outcome and returns what to send now.
    pub(crate) fn offer(&mut self, sequence_no: u32, outcome: SynthesisOutcome) -> Vec<Emitted> {
        let mut emitted = Vec::new();
        if self.sent_finished {
            return emitted;
        }
        match outcome {
            SynthesisOutcome::Audio(pcm) => {
                self.audio.insert(sequence_no, pcm);
            }
            SynthesisOutcome::Failed(reason) => {
                // The client unblocks the moment it learns the number is gone.
                emitted.push(Emitted::Skipped {
                    sequence_no,
                    reason,
                });
                self.done.insert(sequence_no);
            }
        }
        self.drain(&mut emitted);
        self.emit_finished(&mut emitted);
        emitted
    }

    /// No more sentences will arrive for this utterance.
    pub(crate) fn finish(&mut self, total_sentences: u32) -> Vec<Emitted> {
        self.finished = Some(total_sentences);
        let mut emitted = Vec::new();
        self.drain(&mut emitted);
        self.emit_finished(&mut emitted);
        emitted
    }

    fn drain(&mut self, emitted: &mut Vec<Emitted>) {
        while self.done.remove(&self.next) {
            self.next += 1;
        }
        while let Some(pcm) = self.audio.remove(&self.next) {
            self.emit_audio(self.next, pcm, emitted);
            self.next += 1;
            while self.done.remove(&self.next) {
                self.next += 1;
            }
        }
    }

    fn emit_audio(&self, sequence_no: u32, pcm: Vec<u8>, emitted: &mut Vec<Emitted>) {
        if pcm.is_empty() {
            return;
        }
        let mut offset = 0usize;
        while offset < pcm.len() {
            let end = (offset + AUDIO_CHUNK_BYTES).min(pcm.len());
            emitted.push(Emitted::Chunk {
                sequence_no,
                pcm: pcm[offset..end].to_vec(),
                is_final_chunk: end == pcm.len(),
            });
            offset = end;
        }
    }

    fn emit_finished(&mut self, emitted: &mut Vec<Emitted>) {
        if self.sent_finished || !self.audio.is_empty() {
            return;
        }
        let Some(total_sentences) = self.finished else {
            return;
        };
        if self.next < total_sentences {
            return;
        }
        self.sent_finished = true;
        emitted.push(Emitted::Finished { total_sentences });
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UtteranceKey {
    companion: String,
    utterance_id: String,
    generation_id: u64,
}

#[derive(Clone)]
struct SentenceJob {
    sequence_no: u32,
    text: String,
    source: TtsSource,
    voice: String,
}

struct Completion {
    key: UtteranceKey,
    sequence_no: u32,
    outcome: SynthesisOutcome,
}

/// Runs the configured sentences concurrently and releases their results in
/// order. It owns the only HTTP work of the pipeline.
struct Worker {
    hub: VoiceHub,
    done: mpsc::UnboundedSender<Completion>,
    engines: HashMap<String, Arc<dyn TtsEngine>>,
    emitters: HashMap<UtteranceKey, Emitter>,
    pending: HashMap<UtteranceKey, VecDeque<SentenceJob>>,
    in_flight: HashMap<UtteranceKey, usize>,
}

impl Worker {
    fn new(hub: VoiceHub) -> (Self, mpsc::UnboundedReceiver<Completion>) {
        let (done, receiver) = mpsc::unbounded_channel();
        let worker = Self {
            hub,
            done,
            engines: HashMap::new(),
            emitters: HashMap::new(),
            pending: HashMap::new(),
            in_flight: HashMap::new(),
        };
        (worker, receiver)
    }

    async fn run(
        mut self,
        mut jobs: mpsc::UnboundedReceiver<SynthesisJob>,
        mut done: mpsc::UnboundedReceiver<Completion>,
    ) {
        let mut open = true;
        loop {
            let busy = self.in_flight.values().sum::<usize>() > 0;
            if !open && !busy {
                break;
            }
            tokio::select! {
                job = jobs.recv(), if open => match job {
                    Some(job) => self.accept(job).await,
                    None => open = false,
                },
                completion = done.recv() => {
                    if let Some(completion) = completion {
                        self.complete(completion).await;
                    }
                }
            }
        }
    }

    async fn accept(&mut self, job: SynthesisJob) {
        match job {
            SynthesisJob::Sentence {
                companion,
                utterance_id,
                generation_id,
                sequence_no,
                text,
                source,
                voice,
            } => {
                let key = UtteranceKey {
                    companion,
                    utterance_id,
                    generation_id,
                };
                self.emitters.entry(key.clone()).or_default();
                self.pending
                    .entry(key.clone())
                    .or_default()
                    .push_back(SentenceJob {
                        sequence_no,
                        text,
                        source,
                        voice,
                    });
                self.pump(&key);
            }
            SynthesisJob::Finish {
                companion,
                utterance_id,
                generation_id,
                total_sentences,
            } => {
                let key = UtteranceKey {
                    companion,
                    utterance_id,
                    generation_id,
                };
                let emitter = self.emitters.entry(key.clone()).or_default();
                let emitted = emitter.finish(total_sentences);
                self.send(&key, emitted).await;
                self.retire_if_done(&key);
            }
            SynthesisJob::Invalidate {
                companion,
                utterance_id,
                generation_id,
            } => {
                let key = UtteranceKey {
                    companion,
                    utterance_id,
                    generation_id,
                };
                self.emitters.remove(&key);
                self.pending.remove(&key);
            }
        }
    }

    fn pump(&mut self, key: &UtteranceKey) {
        loop {
            let in_flight = self.in_flight.get(key).copied().unwrap_or(0);
            if in_flight >= MAX_SYNTHESIS_SENTENCES {
                break;
            }
            let Some(job) = self.pending.get_mut(key).and_then(VecDeque::pop_front) else {
                break;
            };
            let engine = self.engine(&job.source);
            let request = TtsRequest {
                text: job.text,
                voice: job.voice,
            };
            let key = key.clone();
            let sequence_no = job.sequence_no;
            let done = self.done.clone();
            *self.in_flight.entry(key.clone()).or_insert(0) += 1;
            tokio::spawn(async move {
                let outcome = synthesize(&*engine, request).await;
                let _ = done.send(Completion {
                    key,
                    sequence_no,
                    outcome,
                });
            });
        }
    }

    async fn complete(&mut self, completion: Completion) {
        if let Some(in_flight) = self.in_flight.get_mut(&completion.key) {
            *in_flight = in_flight.saturating_sub(1);
        }
        let Some(emitter) = self.emitters.get_mut(&completion.key) else {
            return;
        };
        let emitted = emitter.offer(completion.sequence_no, completion.outcome);
        self.send(&completion.key, emitted).await;
        self.retire_if_done(&completion.key);
        self.pump(&completion.key);
    }

    fn retire_if_done(&mut self, key: &UtteranceKey) {
        let Some(emitter) = self.emitters.get(key) else {
            return;
        };
        if emitter.sent_finished {
            self.emitters.remove(key);
            self.pending.remove(key);
            self.in_flight.remove(key);
        }
    }

    async fn send(&self, key: &UtteranceKey, emitted: Vec<Emitted>) {
        for item in emitted {
            let event = match item {
                Emitted::Chunk {
                    sequence_no,
                    pcm,
                    is_final_chunk,
                } => audio_event(w::AudioChunkEnvelope {
                    utterance_id: key.utterance_id.clone(),
                    generation_id: key.generation_id,
                    sequence_no,
                    pcm_data: pcm,
                    sample_rate: SAMPLE_RATE,
                    is_final_chunk,
                    ..Default::default()
                }),
                Emitted::Skipped {
                    sequence_no,
                    reason,
                } => audio_event(w::SentenceSkipped {
                    utterance_id: key.utterance_id.clone(),
                    generation_id: key.generation_id,
                    sequence_no,
                    reason,
                    ..Default::default()
                }),
                Emitted::Finished { total_sentences } => audio_event(w::UtteranceFinished {
                    utterance_id: key.utterance_id.clone(),
                    generation_id: key.generation_id,
                    total_sentences,
                    ..Default::default()
                }),
            };
            self.hub
                .deliver(&key.companion, key.generation_id, event)
                .await;
        }
    }

    fn engine(&mut self, source: &TtsSource) -> Arc<dyn TtsEngine> {
        match source.engine_key() {
            Some(key) => self
                .engines
                .entry(key)
                .or_insert_with(|| source.engine())
                .clone(),
            None => source.engine(),
        }
    }
}

/// One sentence's synthesis with the design's timeout. A failure or a timeout
/// becomes `SentenceSkipped`, never silence.
async fn synthesize(engine: &dyn TtsEngine, request: TtsRequest) -> SynthesisOutcome {
    match tokio::time::timeout(SYNTHESIS_TIMEOUT, engine.synthesize(request)).await {
        Err(_) => SynthesisOutcome::Failed("the TTS connection timed out".into()),
        Ok(Err(error)) => SynthesisOutcome::Failed(error),
        Ok(Ok(audio)) => match wire_pcm(audio) {
            Ok(pcm) => SynthesisOutcome::Audio(pcm),
            Err(error) => SynthesisOutcome::Failed(error),
        },
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

fn spawn_worker(receiver: mpsc::UnboundedReceiver<SynthesisJob>, hub: VoiceHub) {
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
            let (worker, done) = Worker::new(hub);
            runtime.block_on(worker.run(receiver, done));
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "the voice worker thread did not start");
    }
}

// ---------------------------------------------------------------------------
// The RPC-facing API.
// ---------------------------------------------------------------------------

/// A microphone stream bound to one client generation.
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
    let activations = store.activations()?;
    let mut active = false;
    let mut matched = false;
    for activation in &activations {
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
    let owned: Vec<(String, u64)> = activations
        .iter()
        .filter(|activation| {
            activation.client_id.as_deref() == Some(request.client_id.as_str())
                && activation.revision == request.activation_revision
        })
        .map(|activation| (activation.companion_id.clone(), activation.revision))
        .collect();
    if owned.is_empty() {
        return Err(HostError::conflict(
            "client_moved: the activation revision is stale",
        ));
    }
    let Some(hub) = world.get_resource::<VoiceHub>().cloned() else {
        return Err(HostError::internal("the voice plugin is not installed"));
    };
    let (subscription_id, receiver, control) = hub.register(&request.client_id);
    for (companion_id, revision) in owned {
        let generation = world
            .get_resource::<Generations>()
            .map(|generations| generations.current(&companion_id))
            .unwrap_or(0);
        hub.publish(
            &companion_id,
            &request.client_id,
            revision,
            None,
            generation,
        );
        if let Some(mut playback) = world.get_resource_mut::<Playback>() {
            playback.publish(
                &companion_id,
                &request.client_id,
                revision,
                None,
                generation,
            );
        }
    }
    Ok(AudioSubscription {
        subscription_id,
        receiver,
        control,
    })
}

/// Closes one client's audio stream. Playback ownership is kept: a reconnect
/// with the same activation revision recovers, a move does not.
pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(hub) = world.get_resource::<VoiceHub>() {
        hub.unregister(client, subscription);
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
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(discarded());
    };
    db::validate_id(&report.client_id)?;
    if store.client(&report.client_id)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let Some(companion) = world
        .get_resource::<Playback>()
        .and_then(|playback| playback.companion_of_utterance(&report.utterance_id))
    else {
        return Ok(discarded());
    };
    let Some(ownership) = world
        .get_resource::<Playback>()
        .and_then(|playback| playback.owner(&companion))
        .cloned()
    else {
        return Ok(discarded());
    };
    if ownership.client_id != report.client_id
        || ownership.activation_revision != report.activation_revision
        || ownership.utterance_id.as_deref() != Some(report.utterance_id.as_str())
        || ownership.generation_id != report.generation_id
    {
        return Ok(discarded());
    }
    // A moved client's report never releases the new generation's floor.
    let current = store
        .activation(&companion)?
        .map(|activation| activation.revision)
        .unwrap_or(0);
    if current != ownership.activation_revision {
        return Ok(discarded());
    }
    match report.status.as_known() {
        Some(w::playback_status_report::Status::PLAYBACK_COMPLETED) => {
            let total_sentences = ownership.total_sentences;
            release(world, &companion);
            broadcast_speech_completed(world, &companion, &report, total_sentences);
            Ok(w::ReportPlaybackStatusResponse {
                acknowledged: true,
                ..Default::default()
            })
        }
        Some(
            w::playback_status_report::Status::PLAYBACK_INTERRUPTED
            | w::playback_status_report::Status::PLAYBACK_FAILED,
        ) => {
            // Interrupted or failed playback frees the floor without claiming
            // that the words were delivered.
            release(world, &companion);
            Ok(w::ReportPlaybackStatusResponse {
                acknowledged: true,
                ..Default::default()
            })
        }
        _ => Ok(discarded()),
    }
}

/// Broads the conversation relay signal: the companion finished speaking.
/// The lead can call this from another path; `report_playback` calls it for a
/// `PLAYBACK_COMPLETED` report.
pub(crate) fn broadcast_speech_completed(
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
    if let Some(mut transcriptions) = world.get_resource_mut::<Transcriptions>()
        && !transcriptions.accept(transcription_id)
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
    let Some(playback) = world.get_resource::<Playback>() else {
        return;
    };
    if playback.owner(companion).is_none() {
        // No client owns this companion's audio: the host stays text-only.
        return;
    }
    let streaming = world
        .get_resource::<ActiveUtterances>()
        .and_then(|utterances| utterances.streaming(companion))
        .cloned();
    let utterance = match streaming {
        Some(utterance) => utterance,
        None => {
            let Ok(source) = tts_source(world) else {
                return;
            };
            let holder = world
                .get_resource::<Floors>()
                .and_then(|floors| floors.holder().map(ToOwned::to_owned));
            if let Some(holder) = holder
                && holder != companion
            {
                // The first companion to speak keeps the floor; a peer's turn
                // output is stopped before it can be recorded.
                stop_turn(world, companion, "speech floor was taken by a peer");
                return;
            }
            let Some(utterance) = begin_utterance(world, companion, source) else {
                return;
            };
            utterance
        }
    };
    let sentences = {
        let Some(mut buffers) = world.get_resource_mut::<SentenceBuffers>() else {
            return;
        };
        let buffer = buffers.entry(companion);
        buffer.push_str(text);
        split_sentences(buffer)
    };
    if sentences.is_empty() {
        return;
    }
    let mut jobs = Vec::with_capacity(sentences.len());
    {
        let Some(mut utterances) = world.get_resource_mut::<ActiveUtterances>() else {
            return;
        };
        let Some(active) = utterances.get_mut(companion) else {
            return;
        };
        for sentence in sentences {
            let sequence_no = active.next_sequence;
            active.next_sequence += 1;
            active.chars += sentence.chars().count() as u32;
            jobs.push(SynthesisJob::Sentence {
                companion: companion.to_owned(),
                utterance_id: active.id.clone(),
                generation_id: active.generation_id,
                sequence_no,
                text: sentence,
                source: active.source.clone(),
                voice: active.voice.clone(),
            });
        }
    }
    let _ = utterance;
    if let Some(queue) = world.get_resource::<VoiceQueue>() {
        for job in jobs {
            queue.send(job);
        }
    }
}

/// Called when a turn settles (the lead wires this into
/// `host::agent::turn_settled`): flush the unterminated remainder and finish
/// the utterance's send side.
pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    let Some(floors) = world.get_resource::<Floors>() else {
        return;
    };
    if floors.holder() != Some(companion) {
        return;
    }
    // A turn that produced no text (an empty or `noop` reply) must not disturb
    // an earlier utterance the client is still playing.
    let Some(utterance) = world
        .get_resource::<ActiveUtterances>()
        .and_then(|utterances| utterances.get(companion))
        .cloned()
    else {
        release(world, companion);
        return;
    };
    if utterance.state != UtteranceState::Streaming {
        return;
    }
    let remainder = {
        let Some(mut buffers) = world.get_resource_mut::<SentenceBuffers>() else {
            return;
        };
        flush_sentences(buffers.entry(companion))
    };
    let mut remainder_jobs = Vec::new();
    let (total, chars);
    {
        let Some(mut utterances) = world.get_resource_mut::<ActiveUtterances>() else {
            return;
        };
        let Some(active) = utterances.get_mut(companion) else {
            return;
        };
        if let Some(text) = remainder {
            let sequence_no = active.next_sequence;
            active.next_sequence += 1;
            active.chars += text.chars().count() as u32;
            remainder_jobs.push((sequence_no, text));
        }
        let state = &mut active.state;
        *state = UtteranceState::Finished;
        total = active.next_sequence;
        chars = active.chars;
    }
    if total == 0 {
        release(world, companion);
        return;
    }
    if let Some(queue) = world.get_resource::<VoiceQueue>() {
        for (sequence_no, text) in remainder_jobs {
            queue.send(SynthesisJob::Sentence {
                companion: companion.to_owned(),
                utterance_id: utterance.id.clone(),
                generation_id: utterance.generation_id,
                sequence_no,
                text,
                source: utterance.source.clone(),
                voice: utterance.voice.clone(),
            });
        }
        queue.send(SynthesisJob::Finish {
            companion: companion.to_owned(),
            utterance_id: utterance.id.clone(),
            generation_id: utterance.generation_id,
            total_sentences: total,
        });
    }
    if let Some(mut playback) = world.get_resource_mut::<Playback>() {
        playback.finish(companion, total, Instant::now() + playback_deadline(chars));
    }
}

/// Flushes the companion's playback. A new utterance or a move invalidates the
/// older generation; the client discards everything up to
/// `flush_generation_id`.
pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(owner) = world
        .get_resource::<Playback>()
        .and_then(|playback| playback.owner(companion))
        .cloned()
    else {
        return Err(HostError::not_found(
            "no client owns this companion's audio output",
        ));
    };
    let generation = world
        .get_resource::<Generations>()
        .map(|generations| generations.current(companion))
        .unwrap_or(0);
    if let Some(mut playback) = world.get_resource_mut::<Playback>()
        && let Some((utterance_id, _)) = playback.take_utterance(companion)
    {
        if let Some(hub) = world.get_resource::<VoiceHub>().cloned() {
            hub.send_to(
                &owner.client_id,
                flush_event(&utterance_id, generation, reason),
            );
        }
        if let Some(queue) = world.get_resource::<VoiceQueue>() {
            queue.send(SynthesisJob::Invalidate {
                companion: companion.to_owned(),
                utterance_id,
                generation_id: generation,
            });
        }
    }
    if let Some(mut generations) = world.get_resource_mut::<Generations>() {
        generations.invalidate(companion);
    }
    clear_utterance_state(world, companion);
    if let Some(hub) = world.get_resource::<VoiceHub>() {
        hub.retire(companion);
    }
    Ok(())
}

/// Called when the active client moved (the lead wires this into
/// `host::client::activate`): the old client is flushed with
/// `FLUSH_REASON_CLIENT_MOVED` and loses playback ownership.
pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str, revision: u64) {
    let Some(playback) = world.get_resource::<Playback>() else {
        return;
    };
    let owns = playback
        .owner(companion)
        .is_some_and(|owner| owner.client_id == client && owner.activation_revision == revision);
    if !owns {
        return;
    }
    let _ = flush(world, companion, w::FlushReason::ClientMoved);
    if let Some(mut playback) = world.get_resource_mut::<Playback>() {
        playback.remove(companion);
    }
    if let Some(hub) = world.get_resource::<VoiceHub>() {
        hub.remove(companion);
    }
}

/// Releases the floor of a companion whose client never reported playback.
pub(crate) fn expire_playback(world: &mut World) {
    let Some(playback) = world.get_resource::<Playback>() else {
        return;
    };
    let now = Instant::now();
    let expired: Vec<String> = playback
        .by_companion
        .iter()
        .filter(|(_, owner)| owner.deadline.is_some_and(|deadline| deadline <= now))
        .map(|(companion, _)| companion.clone())
        .collect();
    for companion in expired {
        tracing::debug!(%companion, "playback did not report; releasing the speech floor");
        release(world, companion.as_str());
    }
}

/// Ends an utterance that produced no audio and frees the floor.
fn release(world: &mut World, companion: &str) {
    let generation = world
        .get_resource::<Generations>()
        .map(|generations| generations.current(companion))
        .unwrap_or(0);
    if let Some(mut playback) = world.get_resource_mut::<Playback>()
        && let Some((utterance_id, _)) = playback.take_utterance(companion)
        && let Some(queue) = world.get_resource::<VoiceQueue>()
    {
        queue.send(SynthesisJob::Invalidate {
            companion: companion.to_owned(),
            utterance_id,
            generation_id: generation,
        });
    }
    if let Some(mut generations) = world.get_resource_mut::<Generations>() {
        generations.invalidate(companion);
    }
    clear_utterance_state(world, companion);
    if let Some(hub) = world.get_resource::<VoiceHub>() {
        hub.retire(companion);
    }
}

fn clear_utterance_state(world: &mut World, companion: &str) {
    if let Some(mut utterances) = world.get_resource_mut::<ActiveUtterances>() {
        utterances.remove(companion);
    }
    if let Some(mut buffers) = world.get_resource_mut::<SentenceBuffers>() {
        buffers.clear(companion);
    }
    if let Some(mut floors) = world.get_resource_mut::<Floors>() {
        floors.release(companion);
    }
}

fn begin_utterance(
    world: &mut World,
    companion: &str,
    source: TtsSource,
) -> Option<ActiveUtterance> {
    let owner = world
        .get_resource::<Playback>()?
        .owner(companion)
        .cloned()?;
    let previous = world
        .get_resource_mut::<Playback>()?
        .take_utterance(companion);
    if let Some((utterance_id, generation_id)) = previous {
        if let Some(hub) = world.get_resource::<VoiceHub>().cloned() {
            hub.send_to(
                &owner.client_id,
                flush_event(&utterance_id, generation_id, w::FlushReason::Cancelled),
            );
        }
        if let Some(queue) = world.get_resource::<VoiceQueue>() {
            queue.send(SynthesisJob::Invalidate {
                companion: companion.to_owned(),
                utterance_id,
                generation_id,
            });
        }
    }
    let generation_id = world
        .get_resource_mut::<Generations>()?
        .invalidate(companion);
    let id = uuid::Uuid::new_v4().to_string();
    let voice = companion_voice(world, companion).unwrap_or_default();
    let utterance = ActiveUtterance {
        id: id.clone(),
        generation_id,
        next_sequence: 0,
        chars: 0,
        state: UtteranceState::Streaming,
        voice,
        source,
    };
    world
        .get_resource_mut::<ActiveUtterances>()?
        .begin(companion, utterance.clone());
    world
        .get_resource_mut::<SentenceBuffers>()?
        .clear(companion);
    world
        .get_resource_mut::<Playback>()?
        .begin(companion, &id, generation_id);
    if let Some(mut floors) = world.get_resource_mut::<Floors>() {
        floors.acquire(companion);
    }
    if let Some(hub) = world.get_resource::<VoiceHub>().cloned() {
        hub.publish(
            companion,
            &owner.client_id,
            owner.activation_revision,
            Some(&id),
            generation_id,
        );
    }
    Some(utterance)
}

fn companion_voice(world: &World, companion: &str) -> Option<String> {
    let store = world.get_resource::<Store>()?;
    store
        .companion(companion)
        .ok()
        .flatten()
        .and_then(|companion| companion.voice_identifier)
}

fn playback_deadline(chars: u32) -> Duration {
    let speech = (f64::from(chars) * 0.18).min(600.0);
    Duration::from_secs_f64(speech) + PLAYBACK_GRACE
}

fn stop_turn(world: &mut World, companion: &str, reason: &str) {
    let Some(agent) = agent::agent_of(world, companion) else {
        return;
    };
    world.trigger(StopTurn {
        entity: agent,
        reason: Some(reason.to_owned()),
    });
}

// ---------------------------------------------------------------------------
// Plugin.
// ---------------------------------------------------------------------------

/// Registers the voice resources, the synthesis worker and the deadline
/// system. The lead adds `voice::VoicePlugin` to the host plugin list.
pub(crate) fn register(app: &mut App) {
    let (jobs, receiver) = VoiceQueue::channel();
    let hub = VoiceHub::default();
    spawn_worker(receiver, hub.clone());
    app.insert_resource(hub)
        .insert_resource(jobs)
        .init_resource::<Generations>()
        .init_resource::<ActiveUtterances>()
        .init_resource::<SentenceBuffers>()
        .init_resource::<Floors>()
        .init_resource::<Playback>()
        .init_resource::<Transcriptions>()
        .add_systems(Update, expire_playback);
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        register(app);
    }
}

/// Test-only engine injection: the unit tests never build a provider client.
#[cfg(test)]
#[derive(Resource, Clone, Default)]
pub(crate) struct TestEngines {
    pub(crate) tts: Option<Arc<dyn TtsEngine>>,
    pub(crate) stt: Option<Arc<dyn SttEngine>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::clients::ClientDeclaration;

    /// A fake TTS engine: deterministic PCM, optionally failing. The tests
    /// never touch the network.
    struct FakeTts {
        fail: bool,
    }

    impl TtsEngine for FakeTts {
        fn synthesize<'a>(
            &'a self,
            request: TtsRequest,
        ) -> BoxFuture<'a, std::result::Result<TtsAudio, String>> {
            Box::pin(async move {
                if self.fail {
                    return Err("mock tts failed".into());
                }
                let mut pcm = Vec::with_capacity(request.text.len() * 2);
                for byte in request.text.bytes() {
                    pcm.extend_from_slice(&i16::from(byte).to_le_bytes());
                }
                Ok(TtsAudio {
                    pcm,
                    sample_rate: SAMPLE_RATE,
                })
            })
        }
    }

    /// A fake STT engine whose transcript names the audio length.
    struct FakeStt;

    impl SttEngine for FakeStt {
        fn transcribe<'a>(
            &'a self,
            request: SttRequest,
        ) -> BoxFuture<'a, std::result::Result<SttResult, String>> {
            Box::pin(async move {
                Ok(SttResult {
                    text: format!("{} bytes", request.pcm.len()),
                })
            })
        }
    }

    fn test_world() -> (World, mpsc::UnboundedReceiver<SynthesisJob>) {
        let mut world = World::new();
        world.insert_resource(VoiceHub::default());
        let (queue, receiver) = VoiceQueue::channel();
        world.insert_resource(queue);
        world.insert_resource(Generations::default());
        world.insert_resource(ActiveUtterances::default());
        world.insert_resource(SentenceBuffers::default());
        world.insert_resource(Floors::default());
        world.insert_resource(Playback::default());
        world.insert_resource(Transcriptions::default());
        world.insert_resource(TestEngines {
            tts: Some(Arc::new(FakeTts { fail: false })),
            stt: Some(Arc::new(FakeStt)),
        });
        (world, receiver)
    }

    fn store_with_client() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let capabilities: Vec<String> = Vec::new();
        let specs: Vec<crate::host::client::CapabilitySpec> = Vec::new();
        store
            .register_client(&ClientDeclaration {
                id: "c1",
                name: "desk",
                platform: "linux",
                capabilities: &capabilities,
                specs: &specs,
                expected_revision: 0,
            })
            .unwrap();
        store.activate("ene", Some("c1"), 0).unwrap();
        (root, store)
    }

    fn claim(world: &mut World, companion: &str, client: &str, revision: u64) {
        world
            .resource_mut::<Playback>()
            .publish(companion, client, revision, None, 0);
    }

    /// Both halves of a subscription. Audio is backpressured; flushes travel
    /// on their own channel so they cannot be lost behind queued audio.
    fn subscribe(
        world: &mut World,
        client: &str,
    ) -> (
        mpsc::Receiver<w::AudioOutputEvent>,
        mpsc::UnboundedReceiver<w::AudioOutputEvent>,
    ) {
        let (_, audio, control) = world.resource::<VoiceHub>().register(client);
        (audio, control)
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
    }

    #[test]
    fn a_long_unpunctuated_run_is_still_spoken() {
        let mut buffer = "あ".repeat(MAX_SENTENCE_CHARS + 5);
        let sentences = split_sentences(&mut buffer);
        assert_eq!(sentences.len(), 1);
        assert_eq!(sentences[0].chars().count(), MAX_SENTENCE_CHARS);
        assert_eq!(buffer.chars().count(), 5);
    }

    #[tokio::test]
    async fn a_new_utterance_invalidates_the_older_generation() {
        let (mut world, _jobs) = test_world();
        let (_audio, mut events) = subscribe(&mut world, "c1");
        claim(&mut world, "ene", "c1", 1);
        on_stream_text(&mut world, "ene", "ひとつめ。");
        let first = world.resource::<Generations>().current("ene");
        assert_eq!(first, 1);

        flush(&mut world, "ene", w::FlushReason::BargeIn).unwrap();
        let event = events.try_recv().expect("a flush reaches the client");
        match event.event {
            Some(w::audio_output_event::Event::Flush(flush)) => {
                assert_eq!(flush.flush_generation_id, first);
                assert_eq!(flush.reason, w::FlushReason::BargeIn);
            }
            other => panic!("expected a flush, got {other:?}"),
        }
        // The ordering gate drops audio of the invalidated generation.
        let stale = audio_event(w::AudioChunkEnvelope {
            utterance_id: "one".into(),
            generation_id: first,
            sequence_no: 0,
            pcm_data: vec![0, 0],
            sample_rate: SAMPLE_RATE,
            ..Default::default()
        });
        assert!(
            !world
                .resource::<VoiceHub>()
                .deliver("ene", first, stale)
                .await
        );

        on_stream_text(&mut world, "ene", "ふたつめ。");
        let second = world.resource::<Generations>().current("ene");
        assert!(second > first);
        let current = audio_event(w::AudioChunkEnvelope {
            utterance_id: "two".into(),
            generation_id: second,
            sequence_no: 0,
            pcm_data: vec![0, 0],
            sample_rate: SAMPLE_RATE,
            ..Default::default()
        });
        assert!(
            world
                .resource::<VoiceHub>()
                .deliver("ene", second, current)
                .await
        );
    }

    #[test]
    fn the_turn_end_flushes_the_unterminated_remainder() {
        let (mut world, mut jobs) = test_world();
        claim(&mut world, "ene", "c1", 1);
        on_stream_text(&mut world, "ene", "ひとつめ。とちゅう");
        let mut sequences = Vec::new();
        while let Ok(job) = jobs.try_recv() {
            if let SynthesisJob::Sentence { sequence_no, .. } = job {
                sequences.push(sequence_no);
            }
        }
        assert_eq!(sequences, vec![0]);

        on_turn_end(&mut world, "ene");
        let mut total = None;
        while let Ok(job) = jobs.try_recv() {
            match job {
                SynthesisJob::Sentence { sequence_no, .. } => sequences.push(sequence_no),
                SynthesisJob::Finish {
                    total_sentences, ..
                } => total = Some(total_sentences),
                SynthesisJob::Invalidate { .. } => {}
            }
        }
        assert_eq!(sequences, vec![0, 1]);
        assert_eq!(total, Some(2));
        let owner = world.resource::<Playback>().owner("ene").unwrap();
        assert_eq!(owner.total_sentences, Some(2));
        assert!(owner.deadline.is_some());
    }

    #[test]
    fn an_empty_follow_up_turn_does_not_cancel_playback() {
        let (mut world, _jobs) = test_world();
        claim(&mut world, "ene", "c1", 1);
        on_stream_text(&mut world, "ene", "ひとつめ。");
        on_turn_end(&mut world, "ene");
        let before = world.resource::<Playback>().owner("ene").unwrap().clone();
        // A turn that says nothing (`noop`) settles without any text delta.
        on_turn_end(&mut world, "ene");
        let after = world.resource::<Playback>().owner("ene").unwrap();
        assert_eq!(after.utterance_id, before.utterance_id);
        assert_eq!(after.generation_id, before.generation_id);
        assert!(world.resource::<Floors>().holder().is_some());
    }

    #[test]
    fn out_of_order_synthesis_is_released_in_sequence_order() {
        let mut emitter = Emitter::default();
        // The second sentence finishes first: nothing is sent yet.
        assert!(
            emitter
                .offer(1, SynthesisOutcome::Audio(vec![2, 0]))
                .is_empty()
        );
        let emitted = emitter.offer(0, SynthesisOutcome::Audio(vec![1, 0]));
        let sequences: Vec<u32> = emitted
            .iter()
            .filter_map(|item| match item {
                Emitted::Chunk { sequence_no, .. } => Some(*sequence_no),
                _ => None,
            })
            .collect();
        assert_eq!(sequences, vec![0, 1]);
        assert!(matches!(
            emitted.as_slice(),
            [
                Emitted::Chunk {
                    is_final_chunk: true,
                    ..
                },
                Emitted::Chunk {
                    is_final_chunk: true,
                    ..
                }
            ]
        ));
        assert!(matches!(
            emitter.finish(2).as_slice(),
            [Emitted::Finished { total_sentences: 2 }]
        ));
    }

    #[test]
    fn a_skipped_sentence_unblocks_the_next_one() {
        let mut emitter = Emitter::default();
        let first = emitter.offer(0, SynthesisOutcome::Failed("timed out".into()));
        assert!(matches!(
            first.as_slice(),
            [Emitted::Skipped { sequence_no: 0, .. }]
        ));
        let second = emitter.offer(1, SynthesisOutcome::Audio(vec![0, 0]));
        assert!(matches!(
            second.as_slice(),
            [Emitted::Chunk {
                sequence_no: 1,
                is_final_chunk: true,
                ..
            }]
        ));
        assert!(matches!(
            emitter.finish(2).as_slice(),
            [Emitted::Finished { total_sentences: 2 }]
        ));
    }

    #[test]
    fn a_stale_playback_report_is_ignored() {
        let (_root, store) = store_with_client();
        let (mut world, _jobs) = test_world();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        claim(&mut world, "ene", "c1", 1);
        on_stream_text(&mut world, "ene", "こんにちは。");
        on_turn_end(&mut world, "ene");
        let owner = world.resource::<Playback>().owner("ene").unwrap().clone();
        let utterance_id = owner.utterance_id.clone().unwrap();

        let stale = w::PlaybackStatusReport {
            utterance_id: utterance_id.clone(),
            generation_id: owner.generation_id + 1,
            status: w::playback_status_report::Status::PLAYBACK_COMPLETED.into(),
            completed_sentence_count: 1,
            client_id: "c1".into(),
            activation_revision: 1,
            ..Default::default()
        };
        let response = report_playback(&mut world, stale).unwrap();
        assert!(!response.acknowledged);
        assert!(
            world
                .resource::<Playback>()
                .owner("ene")
                .unwrap()
                .utterance_id
                .is_some()
        );

        let mut events = world.resource::<Notifier>().subscribe();
        let report = w::PlaybackStatusReport {
            utterance_id,
            generation_id: owner.generation_id,
            status: w::playback_status_report::Status::PLAYBACK_COMPLETED.into(),
            completed_sentence_count: 1,
            client_id: "c1".into(),
            activation_revision: 1,
            ..Default::default()
        };
        let response = report_playback(&mut world, report).unwrap();
        assert!(response.acknowledged);
        assert!(
            world
                .resource::<Playback>()
                .owner("ene")
                .unwrap()
                .utterance_id
                .is_none()
        );
        assert!(world.resource::<Floors>().holder().is_none());
        world.resource_mut::<Notifier>().flush();
        let notification = events.try_recv().expect("speech_completed is broadcast");
        match notification.event {
            Some(w::notification::Event::Generic(event)) => {
                assert_eq!(event.kind, "speech_completed");
            }
            other => panic!("expected speech_completed, got {other:?}"),
        }
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
        assert!(binding.accepts(&chunk));
        assert!(is_still_active(&world, &binding));

        // The companion moved to a new generation.
        store.activate("ene", Some("c1"), 1).unwrap();
        assert!(!is_still_active(&world, &binding));
        let moved = w::AudioInputChunk {
            client_id: "c1".into(),
            activation_revision: 2,
            ..Default::default()
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
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(wav.len(), 52);
    }

    #[tokio::test]
    async fn fake_engines_drive_both_traits_without_a_network() {
        let tts = FakeTts { fail: false };
        let outcome = synthesize(
            &tts,
            TtsRequest {
                text: "ab".into(),
                voice: String::new(),
            },
        )
        .await;
        assert!(matches!(outcome, SynthesisOutcome::Audio(ref pcm) if pcm.len() == 4));

        let failing = FakeTts { fail: true };
        assert!(matches!(
            synthesize(
                &failing,
                TtsRequest {
                    text: "あ".into(),
                    voice: String::new()
                }
            )
            .await,
            SynthesisOutcome::Failed(_)
        ));

        let stt = FakeStt;
        let result = stt
            .transcribe(SttRequest {
                pcm: vec![0; 480],
                sample_rate: SAMPLE_RATE,
            })
            .await
            .unwrap();
        assert_eq!(result.text, "480 bytes");
    }

    #[tokio::test]
    async fn the_worker_orders_audio_and_finishes_the_utterance() {
        let hub = VoiceHub::default();
        let mut events = hub.register("c1").1;
        hub.publish("ene", "c1", 1, Some("u1"), 7);
        let (sentences, receiver) = mpsc::unbounded_channel();
        let (worker, done) = Worker::new(hub);
        let worker = tokio::spawn(worker.run(receiver, done));
        let source = TtsSource::Fixed(Arc::new(FakeTts { fail: false }));
        // The second sentence is queued first: the ordering gate still emits
        // sequence 0 before sequence 1.
        for sequence_no in [1u32, 0] {
            sentences
                .send(SynthesisJob::Sentence {
                    companion: "ene".into(),
                    utterance_id: "u1".into(),
                    generation_id: 7,
                    sequence_no,
                    text: format!("s{sequence_no}"),
                    source: source.clone(),
                    voice: String::new(),
                })
                .unwrap();
        }
        sentences
            .send(SynthesisJob::Finish {
                companion: "ene".into(),
                utterance_id: "u1".into(),
                generation_id: 7,
                total_sentences: 2,
            })
            .unwrap();
        drop(sentences);
        let mut chunks = Vec::new();
        let mut finished = None;
        let collect = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                match event.event {
                    Some(w::audio_output_event::Event::Chunk(chunk)) => {
                        assert_eq!(chunk.generation_id, 7);
                        assert_eq!(chunk.sample_rate, SAMPLE_RATE);
                        chunks.push(chunk.sequence_no);
                    }
                    Some(w::audio_output_event::Event::Finished(event)) => {
                        finished = Some(event.total_sentences);
                        break;
                    }
                    _ => {}
                }
            }
        });
        collect.await.expect("the worker finishes the utterance");
        assert_eq!(chunks, vec![0, 1]);
        assert_eq!(finished, Some(2));
        worker.await.unwrap();
    }
}
