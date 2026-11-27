//! A speech provider plugin behind the host's engine traits.
//!
//! The host already owns the speech floor, the sentence queue and the request
//! deadline; an engine only turns text into audio or audio into text. A plugin
//! engine is therefore the same shape as the built-in one: it asks the plugin
//! for the work, and the plugin reaches its endpoint through the host's
//! broker, so no credential ever enters the guest.
use super::{BoxFuture, HttpResult, SAMPLE_RATE, SetupError, SttEngine, TtsEngine, VoiceHttpError};
use crate::host::{
    model::Credentials,
    providers::{ProviderInvocationContext, Providers},
    settings::{Connection, ConnectionKind},
};
use ene_plugin::{
    AudioFormat, Extension, Operation as PluginOperation, SynthesisRequest, TranscriptionRequest,
};
use ene_plugin_runtime::service::HttpBroker;
use std::sync::Arc;

/// The most audio one utterance may produce. A minute of 24 kHz PCM is about
/// 2.8 MB; this leaves room for a long sentence without letting a provider
/// stream without end.
const MAX_AUDIO_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn speech_extension(kind: ConnectionKind) -> Option<Extension> {
    match kind {
        ConnectionKind::Tts => Some(Extension::Tts),
        ConnectionKind::Stt => Some(Extension::Stt),
        _ => None,
    }
}

/// A speech engine backed by a provider plugin.
pub(crate) struct PluginSpeech {
    running: Arc<crate::host::providers::Running>,
    context: ProviderInvocationContext,
}

impl PluginSpeech {
    /// The plugin engine for `connection`, when a plugin serves its adapter.
    pub(crate) fn new(
        providers: &Providers,
        connection: &Connection,
        credentials: Option<&Credentials>,
        kind: ConnectionKind,
    ) -> std::result::Result<Option<Self>, SetupError> {
        let Some(extension) = speech_extension(kind) else {
            return Ok(None);
        };
        let adapter = connection.endpoint().adapter.clone();
        let Some(running) = providers
            .bound_running(&adapter, extension)
            .map_err(|error| SetupError::Broken(error.to_string()))?
        else {
            return Ok(None);
        };
        let secret = crate::host::model::connection_secret(connection, credentials, providers)
            .map_err(|error| SetupError::Broken(error.to_string()))?;
        let context = ProviderInvocationContext::from_connection(
            &running, connection, secret, providers, None,
        )
        .map_err(|error| SetupError::Broken(error.to_string()))?;
        Ok(Some(Self { running, context }))
    }

    /// Reads a synthesis answer back, chunk by chunk, into one buffer.
    async fn read_audio(broker: &HttpBroker, handle: &str, bytes: u64) -> HttpResult<Vec<u8>> {
        use base64::Engine;
        if bytes > MAX_AUDIO_BYTES {
            return Err(VoiceHttpError::LimitExceeded(
                "the speech provider returned more audio than the host reads".into(),
            ));
        }
        let mut audio = Vec::with_capacity(bytes as usize);
        loop {
            let chunk = broker.read(handle).await.map_err(|error| {
                VoiceHttpError::Transport(format!("the speech provider's audio: {error}"))
            })?;
            audio.extend_from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data_base64)
                    .map_err(|_| {
                        VoiceHttpError::Decode(
                            "the speech provider returned undecodable audio".into(),
                        )
                    })?,
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
            use base64::Engine;
            let request = SynthesisRequest {
                endpoint: self.context.endpoint.clone(),
                options: self.context.options.clone(),
                model: model_of(&self.context.options),
                text,
                voice,
                speed: 0.0,
                format: AudioFormat::Pcm16,
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            let collector = tokio::spawn(async move {
                let mut streamed = Vec::<u8>::new();
                let mut sample_rate = 0u32;
                let mut got_chunks = false;
                while let Some(event) = rx.recv().await {
                    if let ene_plugin::Event::Audio {
                        data_base64,
                        sample_rate: rate,
                        ..
                    } = event
                    {
                        got_chunks = true;
                        if rate > 0 {
                            sample_rate = rate;
                        }
                        if !data_base64.is_empty() {
                            let decoded = base64::engine::general_purpose::STANDARD
                                .decode(&data_base64)
                                .map_err(|_| {
                                    VoiceHttpError::Decode(
                                        "the speech provider emitted undecodable audio chunk"
                                            .into(),
                                    )
                                })?;
                            if (streamed.len() + decoded.len()) as u64 > MAX_AUDIO_BYTES {
                                return Err(VoiceHttpError::LimitExceeded(
                                    "the speech provider exceeded the maximum streamed audio size"
                                        .into(),
                                ));
                            }
                            streamed.extend_from_slice(&decoded);
                        }
                    }
                }
                Ok::<_, VoiceHttpError>((got_chunks, streamed, sample_rate))
            });
            let services = self.context.services();
            let reply = self
                .running
                .invoke_streaming(
                    PluginOperation::Synthesize { request },
                    services.clone(),
                    tx,
                )
                .await
                .map_err(|error| {
                    VoiceHttpError::Transport(format!("the speech provider: {error}"))
                })?;
            let (got_chunks, streamed, streamed_rate) = collector.await.map_err(|_| {
                VoiceHttpError::Transport("the speech stream collector stopped".into())
            })??;
            if got_chunks {
                let rate = if streamed_rate > 0 {
                    streamed_rate
                } else {
                    SAMPLE_RATE
                };
                return Ok((streamed, rate));
            }
            let handle = reply
                .get("handle")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    VoiceHttpError::Protocol("the speech provider returned no audio handle".into())
                })?;
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
            let audio = Self::read_audio(&services.broker, handle, bytes).await?;
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
                endpoint: self.context.endpoint.clone(),
                options: self.context.options.clone(),
                model: model_of(&self.context.options),
                pcm_base64: base64::engine::general_purpose::STANDARD.encode(pcm),
                sample_rate,
                language: None,
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            let collector = tokio::spawn(async move {
                let mut final_text: Option<String> = None;
                let mut partials = Vec::<String>::new();
                while let Some(event) = rx.recv().await {
                    if let ene_plugin::Event::Transcription { text, is_final } = event {
                        let trimmed = text.trim();
                        if is_final {
                            final_text = Some(trimmed.to_owned());
                        } else if !trimmed.is_empty() {
                            partials.push(trimmed.to_owned());
                        }
                    }
                }
                final_text.or_else(|| (!partials.is_empty()).then(|| partials.join(" ")))
            });
            let reply = self
                .running
                .invoke_streaming(
                    PluginOperation::Transcribe { request },
                    self.context.services(),
                    tx,
                )
                .await
                .map_err(|error| {
                    VoiceHttpError::Transport(format!("the speech provider: {error}"))
                })?;
            if let Some(streamed) = collector.await.ok().flatten() {
                return Ok(streamed);
            }
            Ok(reply
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned())
        })
    }
}

/// The model a speech connection names in its resolved options, if any.
fn model_of(options: &serde_json::Value) -> String {
    options
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map_or_default(str::trim)
        .to_owned()
}
