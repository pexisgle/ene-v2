//! Voice service: microphone input (STT), synthesized speech output (TTS) and
//! playback reports. The host owns generations, the audio-owning client and the
//! speech floor (docs/design/voice-avatar.md §4–§6). `StreamAudioInput` binds
//! the stream to the client's activation and recognizes each finished utterance;
//! `SubscribeAudioOutput` only pumps the host's generation-gated channel.
use super::Services;
use crate::host::{
    Runtime,
    voice::{self, InputBinding, SetupError, SttEngine},
};
use connectrpc::{
    ConnectError, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;
use std::sync::Arc;

#[allow(refining_impl_trait)]
impl w::VoiceService for Services {
    async fn stream_audio_input(
        &self,
        _: RequestContext,
        mut inbound: InboundStream<w::AudioInputChunk>,
    ) -> ServiceResult<ServiceStream<w::TranscriptionEvent>> {
        let engine = match self
            .runtime
            .read(|app| Ok(voice::stt_source(app.world())))
            .await
            .map_err(ConnectError::from)?
        {
            Ok(engine) => engine,
            Err(error) => return Err(setup_error(error)),
        };
        let runtime = self.runtime.clone();
        let stream = async_stream::stream! {
            let mut recognition = Recognition {
                sample_rate: voice::SAMPLE_RATE,
                ..Default::default()
            };
            loop {
                let (flush_after, end) =
                    match tokio::time::timeout(voice::STT_SILENCE_WINDOW, inbound.next()).await {
                        Err(_silence) => (true, false),
                        Ok(None) => (true, true),
                        Ok(Some(Err(error))) => {
                            yield Err(error);
                            break;
                        }
                        Ok(Some(Ok(message))) => {
                            let chunk = message.to_owned_message();
                            if recognition.binding.is_none() {
                                let candidate = chunk.clone();
                                match runtime
                                    .read(move |app| Ok(voice::bind_input(app.world(), &candidate)))
                                    .await
                                    .map_err(ConnectError::from)?
                                {
                                    Ok(binding) => recognition.binding = Some(binding),
                                    Err(error) => {
                                        yield Err(ConnectError::from(error));
                                        break;
                                    }
                                }
                            }
                            if !recognition
                                .binding
                                .as_ref()
                                .is_some_and(|binding| binding.accepts(&chunk))
                            {
                                continue;
                            }
                            if chunk.is_companion_speaking {
                                recognition.pending.clear();
                                recognition.speaking = false;
                                recognition.transcription_id.clear();
                                continue;
                            }
                            if chunk.sample_rate != 0 {
                                if !voice::STT_SAMPLE_RATES.contains(&chunk.sample_rate) {
                                    yield Err(ConnectError::invalid_argument(
                                        "audio must be 16 kHz or 24 kHz mono PCM",
                                    ));
                                    break;
                                }
                                if let Some(event) = if chunk.sample_rate
                                    != recognition.sample_rate
                                    && !recognition.pending.is_empty()
                                {
                                    recognition.flush(&runtime, &engine).await?
                                } else {
                                    None
                                } {
                                    yield Ok(event);
                                }
                                recognition.sample_rate = chunk.sample_rate;
                            }
                            if chunk.pcm_data.is_empty() {
                                (true, false)
                            } else {
                                if !recognition.speaking {
                                    recognition.speaking = true;
                                    if recognition.transcription_id.is_empty() {
                                        recognition.transcription_id =
                                            uuid::Uuid::new_v4().to_string();
                                    }
                                    yield Ok(w::TranscriptionEvent {
                                        transcription_id: recognition.transcription_id.clone(),
                                        is_speaking: true,
                                        ..Default::default()
                                    });
                                }
                                recognition.pending.extend_from_slice(&chunk.pcm_data);
                                (
                                    recognition.pending.len() >= voice::MAX_STT_BYTES,
                                    false,
                                )
                            }
                        }
                    };
                if let Some(event) = if flush_after {
                    recognition.flush(&runtime, &engine).await?
                } else {
                    None
                } {
                    yield Ok(event);
                }
                if end {
                    break;
                }
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn subscribe_audio_output(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeAudioRequest>,
    ) -> ServiceResult<ServiceStream<w::AudioOutputEvent>> {
        if let Err(error) = self
            .runtime
            .read(|app| Ok(voice::tts_source(app.world())))
            .await
            .map_err(ConnectError::from)?
        {
            return Err(setup_error(error));
        }
        let request = request.to_owned_message();
        let voice::AudioSubscription {
            subscription_id,
            mut receiver,
            mut control,
        } = self
            .runtime
            .read({
                let request = request.clone();
                move |app| voice::subscribe_audio(app.world_mut(), request)
            })
            .await
            .map_err(ConnectError::from)?;
        let guard = StreamGuard {
            runtime: self.runtime.clone(),
            client_id: request.client_id,
            subscription: subscription_id,
        };
        let stream = async_stream::stream! {
            loop {
                let event = tokio::select! {
                    event = receiver.recv() => event,
                    event = control.recv() => event,
                };
                let Some(event) = event else { break };
                yield Ok(event);
            }
            drop(guard);
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn report_playback_status(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::PlaybackStatusReport>,
    ) -> ServiceResult<w::ReportPlaybackStatusResponse> {
        match self
            .runtime
            .read(|app| {
                Ok(voice::configured(
                    app.world(),
                    crate::host::settings::ConnectionKind::Tts,
                ))
            })
            .await
            .map_err(ConnectError::from)?
        {
            Ok(()) | Err(SetupError::Broken(_)) | Err(SetupError::Unsupported(_)) => {}
            Err(error) => return Err(setup_error(error)),
        }
        let report = request.to_owned_message();
        self.call(move |app| voice::report_playback(app.world_mut(), report))
            .await
    }
}

fn setup_error(error: SetupError) -> ConnectError {
    match error {
        SetupError::Missing => ConnectError::unimplemented(voice::VOICE_UNAVAILABLE),
        SetupError::Disabled(message) | SetupError::Unsupported(message) => {
            ConnectError::failed_precondition(message)
        }
        SetupError::Broken(message) => ConnectError::unavailable(message),
    }
}

#[derive(Default)]
struct Recognition {
    binding: Option<InputBinding>,
    pending: Vec<u8>,
    sample_rate: u32,
    transcription_id: String,
    speaking: bool,
}

impl Recognition {
    async fn flush(
        &mut self,
        runtime: &Runtime,
        engine: &Arc<dyn SttEngine>,
    ) -> std::result::Result<Option<w::TranscriptionEvent>, ConnectError> {
        self.speaking = false;
        if self.pending.len() < voice::MIN_STT_BYTES {
            self.pending.clear();
            return Ok(None);
        }
        let pcm = std::mem::take(&mut self.pending);
        let Some(binding) = self.binding.clone() else {
            return Ok(None);
        };
        let active = runtime
            .read({
                let binding = binding.clone();
                move |app| Ok(voice::is_still_active(app.world(), &binding))
            })
            .await
            .map_err(ConnectError::from)?;
        if !active {
            return Ok(None);
        }
        let text =
            tokio::time::timeout(voice::STT_TIMEOUT, engine.transcribe(pcm, self.sample_rate))
                .await
                .map_err(|_| ConnectError::deadline_exceeded("speech recognition timed out"))?
                .map_err(|error| {
                    ConnectError::unavailable(format!("speech recognition failed: {error}"))
                })?;
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.transcription_id.clear();
            return Ok(None);
        }
        let transcription_id = std::mem::take(&mut self.transcription_id);
        let accepted = runtime
            .call({
                let id = transcription_id.clone();
                let text = text.clone();
                move |app| voice::accept_transcription(app.world_mut(), &id, &text, &binding)
            })
            .await
            .map_err(ConnectError::from)?;
        if !accepted {
            return Ok(None);
        }
        Ok(Some(w::TranscriptionEvent {
            transcription_id,
            text,
            is_final: true,
            is_speaking: false,
            ..Default::default()
        }))
    }
}

struct StreamGuard {
    runtime: Runtime,
    client_id: String,
    subscription: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let runtime = self.runtime.clone();
        let client_id = self.client_id.clone();
        let subscription = self.subscription.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime
                    .read(move |app| {
                        voice::unsubscribe_audio(app.world(), &client_id, &subscription);
                        Ok(())
                    })
                    .await;
            });
        }
    }
}
