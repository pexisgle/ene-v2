//! A speech provider plugin behind the host's engine traits.
//!
//! The host already owns the speech floor, the sentence queue and the request
//! deadline; an engine only turns text into audio or audio into text. A plugin
//! engine is therefore the same shape as the built-in one: it asks the plugin
//! for the work, and the plugin reaches its endpoint through the host's
//! broker, so no credential ever enters the guest.
use super::{BoxFuture, HttpResult, SAMPLE_RATE, SetupError, SttEngine, TtsEngine};
use crate::host::{
    model::Credentials,
    providers::{BrokerServices, Grant, Providers},
    settings::{Connection, ConnectionKind},
};
use ene_plugin::{
    AudioFormat, Extension, Operation as PluginOperation, SynthesisRequest, TranscriptionRequest,
};
use ene_plugin_runtime::{instance::Services, service::HttpBroker};
use std::sync::Arc;

/// The most audio one utterance may produce. A minute of 24 kHz PCM is about
/// 2.8 MB; this leaves room for a long sentence without letting a provider
/// stream without end.
const MAX_AUDIO_BYTES: u64 = 16 * 1024 * 1024;

/// A speech engine backed by a provider plugin.
pub(crate) struct PluginSpeech {
    running: Arc<crate::host::providers::Running>,
    broker: Arc<HttpBroker>,
    connection: Connection,
}

impl PluginSpeech {
    /// The plugin engine for `connection`, when a plugin serves its adapter.
    pub(crate) fn new(
        providers: &Providers,
        connection: &Connection,
        credentials: Option<&Credentials>,
        kind: ConnectionKind,
    ) -> Result<Option<Self>, SetupError> {
        let extension = match kind {
            ConnectionKind::Tts => Extension::Tts,
            ConnectionKind::Stt => Extension::Stt,
            _ => return Ok(None),
        };
        let adapter = connection.endpoint().adapter.clone();
        let Some(running) = providers.running(&adapter, extension) else {
            return Ok(None);
        };
        let grant = Grant::new(running.provider(), connection, credentials, providers)
            .map_err(|error| SetupError::Broken(error.to_string()))?;
        Ok(Some(Self {
            running,
            broker: crate::host::providers::http_broker(Arc::new(grant))
                .map_err(|error| SetupError::Broken(format!("plugin HTTP transport: {error}")))?,
            connection: connection.clone(),
        }))
    }

    /// The services a speech call needs: the broker, and nothing else.
    fn services(&self) -> Arc<dyn Services> {
        BrokerServices::new(Arc::clone(&self.broker), self.running.provider().id.clone())
    }

    /// Reads a synthesis answer back, chunk by chunk, into one buffer.
    async fn read_audio(&self, handle: &str, bytes: u64) -> HttpResult<Vec<u8>> {
        use base64::Engine;
        if bytes > MAX_AUDIO_BYTES {
            return Err("the speech provider returned more audio than the host reads".into());
        }
        let mut audio = Vec::with_capacity(bytes as usize);
        loop {
            let chunk = self
                .broker
                .read(handle)
                .await
                .map_err(|error| format!("the speech provider's audio: {error}"))?;
            audio.extend_from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data_base64)
                    .map_err(|_| "the speech provider returned undecodable audio".to_owned())?,
            );
            if chunk.done {
                return Ok(audio);
            }
        }
    }
}

impl TtsEngine for PluginSpeech {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            let options = options_of(&self.connection);
            let request = SynthesisRequest {
                endpoint: self.running.provider().endpoint.clone(),
                options,
                model: model_of(&self.connection),
                text,
                voice,
                speed: 0.0,
                format: AudioFormat::Pcm16,
            };
            let reply = self
                .running
                .invoke(PluginOperation::Synthesize { request }, self.services())
                .await
                .map_err(|error| format!("the speech provider: {error}"))?;
            let handle = reply
                .get("handle")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "the speech provider returned no audio handle".to_owned())?;
            let bytes = reply
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let rate = reply
                .get("sample_rate")
                .and_then(serde_json::Value::as_u64)
                .and_then(|rate| u32::try_from(rate).ok())
                .filter(|rate| *rate > 0)
                .unwrap_or(SAMPLE_RATE);
            let audio = self.read_audio(handle, bytes).await?;
            Ok((audio, rate))
        })
    }
}

impl SttEngine for PluginSpeech {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move {
            use base64::Engine;
            let request = TranscriptionRequest {
                endpoint: self.running.provider().endpoint.clone(),
                options: options_of(&self.connection),
                model: model_of(&self.connection),
                pcm_base64: base64::engine::general_purpose::STANDARD.encode(pcm),
                sample_rate,
                language: None,
            };
            let reply = self
                .running
                .invoke(PluginOperation::Transcribe { request }, self.services())
                .await
                .map_err(|error| format!("the speech provider: {error}"))?;
            Ok(reply
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned())
        })
    }
}

/// The model a speech connection names, if any.
fn model_of(connection: &Connection) -> String {
    connection.endpoint().options.clone()
}

/// The connection's own options document, as the plugin declared it.
fn options_of(connection: &Connection) -> serde_json::Value {
    serde_json::from_str(&connection.endpoint().options).unwrap_or(serde_json::Value::Null)
}
