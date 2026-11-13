//! Voice service: microphone input (STT), synthesized speech output (TTS) and
//! playback reports. The host owns the generations, the audio-owning client
//! and the speech floor; see docs/design/voice-avatar.md §4–§6.
//!
//! `StreamAudioInput` runs speech recognition where the bidirectional request
//! already lives: the Connect layer consumes the provisional request stream,
//! binds it to the client's activation, and sends each finished utterance to
//! the configured STT connection. `SubscribeAudioOutput` only pumps the host's
//! generation-gated audio channel.
use super::Services;
use crate::host::{
    Runtime,
    voice::{self, SetupError, SttEngine},
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
        let source = match self
            .runtime
            .read(|app| Ok(voice::stt_source(app.world())))
            .await
            .map_err(ConnectError::from)?
        {
            Ok(source) => source,
            Err(error) => return Err(setup_error(error)),
        };
        let engine = source.engine();
        let runtime = self.runtime.clone();
        let stream = async_stream::stream! {
            let mut binding: Option<voice::InputBinding> = None;
            let mut pending: Vec<u8> = Vec::new();
            let mut sample_rate = voice::SAMPLE_RATE;
            let mut transcription_id = String::new();
            let mut speaking = false;
            loop {
                let waited = tokio::time::timeout(voice::STT_SILENCE_WINDOW, inbound.next()).await;
                let chunk = match waited {
                    Err(_silence) => {
                        // A pause ends the utterance even if the client keeps
                        // the stream open.
                        match flush(
                            &runtime,
                            &engine,
                            binding.as_ref(),
                            &mut pending,
                            sample_rate,
                            &mut transcription_id,
                            &mut speaking,
                        ).await {
                            Ok(Some(event)) => yield Ok(event),
                            Ok(None) => {}
                            Err(error) => {
                                yield Err(error);
                                break;
                            }
                        }
                        continue;
                    }
                    Ok(None) => {
                        match flush(
                            &runtime,
                            &engine,
                            binding.as_ref(),
                            &mut pending,
                            sample_rate,
                            &mut transcription_id,
                            &mut speaking,
                        ).await {
                            Ok(Some(event)) => yield Ok(event),
                            Ok(None) => {}
                            Err(error) => yield Err(error),
                        }
                        break;
                    }
                    Ok(Some(Err(error))) => {
                        yield Err(error);
                        break;
                    }
                    Ok(Some(Ok(message))) => message.to_owned_message(),
                };
                if binding.is_none() {
                    let candidate = chunk.clone();
                    match runtime
                        .read(move |app| Ok(voice::bind_input(app.world(), &candidate)))
                        .await
                        .map_err(ConnectError::from)?
                    {
                        Ok(bound) => binding = Some(bound),
                        Err(error) => {
                            yield Err(ConnectError::from(error));
                            break;
                        }
                    }
                }
                let Some(bound) = binding.as_ref() else {
                    continue;
                };
                if !bound.accepts(&chunk) {
                    // A stale generation's chunk is discarded silently.
                    continue;
                }
                if chunk.is_companion_speaking {
                    // Frames captured while the companion is playing are never
                    // treated as user speech or as approval evidence.
                    pending.clear();
                    speaking = false;
                    transcription_id.clear();
                    continue;
                }
                if chunk.sample_rate != 0 {
                    if !voice::STT_SAMPLE_RATES.contains(&chunk.sample_rate) {
                        yield Err(ConnectError::invalid_argument(
                            "audio must be 16 kHz or 24 kHz mono PCM",
                        ));
                        break;
                    }
                    if chunk.sample_rate != sample_rate && !pending.is_empty() {
                        match flush(
                            &runtime,
                            &engine,
                            binding.as_ref(),
                            &mut pending,
                            sample_rate,
                            &mut transcription_id,
                            &mut speaking,
                        ).await {
                            Ok(Some(event)) => yield Ok(event),
                            Ok(None) => {}
                            Err(error) => {
                                yield Err(error);
                                break;
                            }
                        }
                    }
                    sample_rate = chunk.sample_rate;
                }
                if chunk.pcm_data.is_empty() {
                    // An empty chunk is the client's explicit end of speech.
                    match flush(
                        &runtime,
                        &engine,
                        binding.as_ref(),
                        &mut pending,
                        sample_rate,
                        &mut transcription_id,
                        &mut speaking,
                    ).await {
                        Ok(Some(event)) => yield Ok(event),
                        Ok(None) => {}
                        Err(error) => {
                            yield Err(error);
                            break;
                        }
                    }
                    continue;
                }
                if !speaking {
                    if transcription_id.is_empty() {
                        transcription_id = uuid::Uuid::new_v4().to_string();
                    }
                    speaking = true;
                    yield Ok(w::TranscriptionEvent {
                        transcription_id: transcription_id.clone(),
                        is_speaking: true,
                        ..Default::default()
                    });
                }
                pending.extend_from_slice(&chunk.pcm_data);
                if pending.len() >= voice::MAX_STT_BYTES {
                    match flush(
                        &runtime,
                        &engine,
                        binding.as_ref(),
                        &mut pending,
                        sample_rate,
                        &mut transcription_id,
                        &mut speaking,
                    ).await {
                        Ok(Some(event)) => yield Ok(event),
                        Ok(None) => {}
                        Err(error) => {
                            yield Err(error);
                            break;
                        }
                    }
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
        // A host without a usable TTS connection never pretends to speak.
        match self
            .runtime
            .read(|app| Ok(voice::tts_source(app.world())))
            .await
            .map_err(ConnectError::from)?
        {
            Ok(_) => {}
            Err(error) => return Err(setup_error(error)),
        }
        let request = request.to_owned_message();
        let subscription = self
            .runtime
            .read({
                let request = request.clone();
                move |app| voice::subscribe_audio(app.world_mut(), request)
            })
            .await
            .map_err(ConnectError::from)?;
        let voice::AudioSubscription {
            subscription_id,
            mut receiver,
            mut control,
        } = subscription;
        let guard = StreamGuard {
            runtime: self.runtime.clone(),
            client_id: request.client_id.clone(),
            subscription: subscription_id,
        };
        let stream = async_stream::stream! {
            loop {
                // Audio waits for the network; control (a flush) never waits.
                let event = tokio::select! {
                    event = receiver.recv() => event,
                    event = control.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
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
        // A report only needs the output capability to exist, not to be
        // healthy: a broken engine must not strand the speech floor.
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

/// Maps a setup failure onto the wire. A missing engine stays
/// `unimplemented`; a disabled one is a failed precondition; a broken one is
/// unavailable. None of them is a fake success.
fn setup_error(error: SetupError) -> ConnectError {
    match error {
        SetupError::Missing => ConnectError::unimplemented(voice::VOICE_UNAVAILABLE),
        SetupError::Disabled(message) | SetupError::Unsupported(message) => {
            ConnectError::failed_precondition(message)
        }
        SetupError::Broken(message) => ConnectError::unavailable(message),
    }
}

/// Flushes the accumulated utterance into one final transcription event, or
/// nothing when the audio was noise. A recognition failure is explicit.
#[allow(clippy::too_many_arguments)]
async fn flush(
    runtime: &Runtime,
    engine: &Arc<dyn SttEngine>,
    binding: Option<&voice::InputBinding>,
    pending: &mut Vec<u8>,
    sample_rate: u32,
    transcription_id: &mut String,
    speaking: &mut bool,
) -> std::result::Result<Option<w::TranscriptionEvent>, ConnectError> {
    *speaking = false;
    if pending.len() < voice::MIN_STT_BYTES {
        pending.clear();
        return Ok(None);
    }
    let pcm = std::mem::take(pending);
    let Some(binding) = binding else {
        return Ok(None);
    };
    let binding = binding.clone();
    let binding_now = binding.clone();
    let active = runtime
        .read(move |app| Ok(voice::is_still_active(app.world(), &binding_now)))
        .await
        .map_err(ConnectError::from)?;
    if !active {
        // The client moved: the old generation's audio is discarded.
        return Ok(None);
    }
    let request = voice::SttRequest { pcm, sample_rate };
    let recognized =
        match tokio::time::timeout(voice::STT_TIMEOUT, engine.transcribe(request)).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return Err(ConnectError::unavailable(format!(
                    "speech recognition failed: {error}"
                )));
            }
            Err(_) => {
                return Err(ConnectError::deadline_exceeded(
                    "speech recognition timed out",
                ));
            }
        };
    let text = recognized.text.trim().to_owned();
    if text.is_empty() {
        transcription_id.clear();
        return Ok(None);
    }
    let id = std::mem::take(transcription_id);
    let client_id = binding.client_id.clone();
    let delivered = runtime
        .call({
            let id = id.clone();
            let text = text.clone();
            move |app| voice::accept_transcription(app.world_mut(), &id, &text, &client_id)
        })
        .await
        .map_err(ConnectError::from)?;
    let _ = delivered;
    Ok(Some(w::TranscriptionEvent {
        transcription_id: id,
        text,
        is_final: true,
        is_speaking: false,
        ..Default::default()
    }))
}

/// Closes the audio stream when the response stream is dropped, including a
/// client disconnect.
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
