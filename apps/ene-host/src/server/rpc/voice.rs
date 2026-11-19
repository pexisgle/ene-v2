//! Voice service: microphone input (STT) and synthesized speech output (TTS).
//! The host owns generations, the audio-owning client and the speech floor
//! (docs/design/voice-avatar.md §4–§6). `StreamAudioInput` binds the stream to
//! the client that opened it and recognizes each finished utterance;
//! `SubscribeAudioOutput` only pumps the host's generation-gated channel.
use super::Services;
use crate::host::{
    Runtime,
    db::Store,
    error::HostError,
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
                            if recognition.binding.is_none() {
                                let candidate = message.clone();
                                match runtime
                                    .read(move |app| {
                                        let chunk = candidate.view();
                                        Ok(voice::bind_input(app.world(), chunk.client_id))
                                    })
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
                            let chunk = message.view();
                            if !recognition
                                .binding
                                .as_ref()
                                .is_some_and(|binding| binding.accepts(chunk.client_id))
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
                                recognition.pending.extend_from_slice(chunk.pcm_data);
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
        let request = request.to_owned_view();
        let (
            voice::AudioSubscription {
                subscription_id,
                mut receiver,
                mut control,
            },
            client_id,
        ) = self
            .runtime
            .read(move |app| {
                let request = request.reborrow();
                let subscription = voice::subscribe_audio(app.world_mut(), request.client_id)?;
                Ok((subscription, request.client_id.to_owned()))
            })
            .await
            .map_err(ConnectError::from)?;
        let guard = StreamGuard {
            runtime: self.runtime.clone(),
            client_id,
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

    async fn speak(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SpeakRequest>,
    ) -> ServiceResult<w::Empty> {
        let client_id = request.client_id.to_owned();
        let text = request.text.trim().to_owned();
        // Validation at the trust boundary, so a malformed probe never reaches
        // the network as a 503 from the engine.
        if text.is_empty() {
            return Err(ConnectError::invalid_argument("text is required"));
        }
        if text.chars().count() > voice::MAX_PROBE_CHARS {
            return Err(ConnectError::invalid_argument(format!(
                "text is limited to {} characters",
                voice::MAX_PROBE_CHARS
            )));
        }
        // Synthesis is a network call, so it happens here rather than on the
        // ECS thread; only the store lookups and the dispatch run inside it.
        let source = self
            .runtime
            .read(|app| {
                voice::tts_source(app.world()).map_err(|error| match error {
                    SetupError::Missing => {
                        HostError::FailedPrecondition(voice::VOICE_UNAVAILABLE.into())
                    }
                    SetupError::Disabled(message)
                    | SetupError::Broken(message)
                    | SetupError::Unsupported(message) => HostError::FailedPrecondition(message),
                })
            })
            .await
            .map_err(ConnectError::from)?;
        let (voice_name, companion) = self
            .runtime
            .read({
                let client_id = client_id.clone();
                move |app| {
                    let store = app.world().resource::<Store>();
                    let companion = store.activation_for_client(&client_id)?.ok_or_else(|| {
                        HostError::FailedPrecondition(
                            "the client is not the active client of any companion".into(),
                        )
                    })?;
                    let voice = store
                        .companion(&companion)?
                        .and_then(|companion| companion.voice_identifier)
                        .unwrap_or_default();
                    Ok((voice, companion))
                }
            })
            .await
            .map_err(ConnectError::from)?;
        let (pcm, rate) = voice::synthesize_for_probe(source, &text, voice_name)
            .await
            .map_err(ConnectError::unavailable)?;
        self.runtime
            .read(move |app| voice::play_once(app.world(), &companion, pcm, rate))
            .await
            .map_err(ConnectError::from)
            .map(Response::ok)?
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
        let was_speaking = self.speaking;
        self.speaking = false;
        fn clear_speaking(
            was_speaking: bool,
            transcription_id: &mut String,
        ) -> Option<w::TranscriptionEvent> {
            let id = std::mem::take(transcription_id);
            was_speaking.then(|| w::TranscriptionEvent {
                transcription_id: id,
                text: String::new(),
                is_final: false,
                is_speaking: false,
                ..Default::default()
            })
        }
        if self.pending.len() < voice::MIN_STT_BYTES {
            self.pending.clear();
            return Ok(clear_speaking(was_speaking, &mut self.transcription_id));
        }
        let pcm = std::mem::take(&mut self.pending);
        let Some(binding) = self.binding.clone() else {
            return Ok(clear_speaking(was_speaking, &mut self.transcription_id));
        };
        if !voiced(&pcm, self.sample_rate) {
            return Ok(clear_speaking(was_speaking, &mut self.transcription_id));
        }
        let active = runtime
            .read({
                let binding = binding.clone();
                move |app| Ok(voice::is_still_active(app.world(), &binding))
            })
            .await
            .map_err(ConnectError::from)?;
        if !active {
            return Ok(clear_speaking(was_speaking, &mut self.transcription_id));
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
            return Ok(clear_speaking(was_speaking, &mut self.transcription_id));
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
            return Ok(was_speaking.then(|| w::TranscriptionEvent {
                transcription_id,
                text: String::new(),
                is_final: false,
                is_speaking: false,
                ..Default::default()
            }));
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

/// Checks for signal above the floor in a 40 ms window so quiet words survive
/// silence at either end of the capture. Removing the mean ignores DC offset.
fn voiced(pcm: &[u8], sample_rate: u32) -> bool {
    let samples = pcm.as_chunks::<2>().0;
    let window = sample_rate as usize / 25;
    let min_variance = f64::from(voice::MIN_STT_RMS).powi(2);
    samples.chunks_exact(window).any(|frame| {
        let (sum, squares) = frame.iter().fold((0.0, 0.0), |(sum, squares), pair| {
            let sample = f64::from(i16::from_le_bytes(*pair)) / 32_768.0;
            (sum + sample, squares + sample * sample)
        });
        let mean = sum / window as f64;
        squares / window as f64 - mean * mean >= min_variance
    })
}

struct StreamGuard {
    runtime: Runtime,
    client_id: String,
    subscription: String,
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        let client_id = self.client_id.clone();
        let subscription = self.subscription.clone();
        drop(self.runtime.read(move |app| {
            voice::unsubscribe_audio(app.world(), &client_id, &subscription);
            Ok(())
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::{Store, clients::ClientDeclaration};
    use bevy_ecs::prelude::{ResMut, Resource};
    use rig_ecs::prelude::Update;
    use tokio::sync::mpsc::error::TryRecvError;

    type Captured = Arc<std::sync::Mutex<Vec<(u32, Vec<i16>)>>>;

    struct RecordingStt(Captured);

    impl SttEngine for RecordingStt {
        fn transcribe<'a>(
            &'a self,
            pcm: Vec<u8>,
            sample_rate: u32,
        ) -> voice::BoxFuture<'a, std::result::Result<String, String>> {
            Box::pin(async move {
                let samples = pcm
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect();
                let mut received = self.0.lock().unwrap();
                received.push((sample_rate, samples));
                Ok(format!("capture {}", received.len()))
            })
        }
    }

    #[tokio::test]
    async fn microphone_stream_keeps_binding_echo_and_rate_boundaries() {
        use connectrpc::StreamMessage;
        use w::VoiceService;

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recording = RecordingStt(captured.clone());
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(move |app| {
                let store = app.world().resource::<Store>();
                store.register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.activate("ene", Some("c1"))?;
                app.world_mut().insert_resource(voice::Engines {
                    stt: Some(Arc::new(recording)),
                    ..Default::default()
                });
                Ok(())
            })
            .await
            .unwrap();
        // Alternating ±amplitude: real energy, unlike a constant offset, so a
        // fixture reaches `transcribe` only when it is voiced enough to. Each
        // chunk is one whole `MIN_STT_BYTES` capture at 24 kHz.
        const SAMPLES: usize = voice::MIN_STT_BYTES / 2;
        let wave = |amplitude: i16| -> Vec<i16> {
            (0..SAMPLES)
                .map(|index| {
                    if index % 2 == 0 {
                        amplitude
                    } else {
                        -amplitude
                    }
                })
                .collect()
        };
        let chunk = |client: &str, rate, samples: Vec<i16>, speaking| {
            let pcm: Vec<u8> = samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            StreamMessage::from_message(&w::AudioInputChunk {
                client_id: client.into(),
                sample_rate: rate,
                pcm_data: pcm,
                is_companion_speaking: speaking,
                ..Default::default()
            })
        };
        let mut quiet = wave(150);
        quiet.resize(SAMPLES * 4, 0); // 600 ms of trailing silence
        let messages = vec![
            chunk("c1", 24_000, wave(5_000), false),
            // A frame that names another client never joins this stream.
            chunk("other", 24_000, wave(5_500), false),
            chunk("c1", 24_000, wave(5_200), false),
            // Playback echo discards the capture in progress.
            chunk("", 0, wave(5_400), true),
            chunk("", 16_000, wave(5_000), false),
            chunk("", 24_000, quiet.clone(), false),
            StreamMessage::from_message(&w::AudioInputChunk::default()),
        ];
        let service = Services {
            runtime: runtime.clone(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        };
        let response = service
            .stream_audio_input(
                RequestContext::new(http::HeaderMap::new()),
                Box::pin(futures_util::stream::iter(messages.into_iter().map(Ok))),
            )
            .await
            .unwrap();
        let events: Vec<_> = response.body.collect().await;
        let finals: Vec<_> = events
            .into_iter()
            .map(Result::unwrap)
            .filter(|event| event.is_final)
            .map(|event| event.text)
            .collect();
        assert_eq!(finals, ["capture 1", "capture 2"]);
        {
            let captured = captured.lock().unwrap();
            assert_eq!(
                captured
                    .iter()
                    .map(|(rate, samples)| (*rate, samples.len()))
                    .collect::<Vec<_>>(),
                [(16_000, SAMPLES), (24_000, quiet.len())]
            );
            // The alternation survives the trip, so the fixture is the signal the
            // client would have sent, not a constant the gate now rejects.
            assert!(
                captured[0]
                    .1
                    .iter()
                    .all(|sample| *sample == -5_000 || *sample == 5_000)
            );
            assert_eq!(captured[1].1, quiet);
        }
        runtime.shutdown().await.unwrap();
    }

    #[derive(Resource, Default)]
    struct Frames(u64);

    #[tokio::test]
    async fn audio_guard_cleanup_outside_tokio_preserves_replacement_without_a_frame() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                app.insert_resource(Frames::default())
                    .add_systems(Update, |mut frames: ResMut<Frames>| frames.0 += 1);
                let store = app.world().resource::<Store>();
                store.register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.activate("ene", Some("c1"))?;
                Ok(())
            })
            .await
            .unwrap();
        let request = || w::SubscribeAudioRequest {
            client_id: "c1".into(),
            ..Default::default()
        };
        let first = request();
        let old = runtime
            .read(move |app| voice::subscribe_audio(app.world_mut(), &first.client_id))
            .await
            .unwrap();
        let old_guard = StreamGuard {
            runtime: runtime.clone(),
            client_id: "c1".into(),
            subscription: old.subscription_id,
        };
        let second = request();
        let mut current = runtime
            .read(move |app| voice::subscribe_audio(app.world_mut(), &second.client_id))
            .await
            .unwrap();
        let current_guard = StreamGuard {
            runtime: runtime.clone(),
            client_id: "c1".into(),
            subscription: current.subscription_id,
        };
        std::thread::spawn(move || drop(old_guard)).join().unwrap();
        runtime.read(|_| Ok(())).await.unwrap();
        assert!(matches!(
            current.receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(matches!(
            current.control.try_recv(),
            Err(TryRecvError::Empty)
        ));

        let check = runtime.clone();
        let (before, after) = runtime
            .read(move |app| {
                let before = app.world().resource::<Frames>().0;
                drop(current_guard);
                let after = check.read(|app| Ok(app.world().resource::<Frames>().0));
                Ok((before, after))
            })
            .await
            .unwrap();
        assert_eq!(after.await.unwrap(), before);
        assert!(matches!(
            current.receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        assert!(matches!(
            current.control.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        runtime.call(|_| Ok(())).await.unwrap();
        assert!(
            runtime
                .read(|app| Ok(app.world().resource::<Frames>().0))
                .await
                .unwrap()
                > before
        );
        runtime.shutdown().await.unwrap();
    }

    /// Whisper-family engines answer silence with a stock phrase (the classic
    /// "ご視聴ありがとうございました"). A buffer at the noise floor, or too short
    /// to be an utterance, must never reach the engine.
    #[test]
    fn only_a_voiced_buffer_reaches_the_recognizer() {
        let samples = |amplitude: i16, count: usize| -> Vec<u8> {
            (0..count)
                .flat_map(|index| {
                    let value = if index % 2 == 0 {
                        amplitude
                    } else {
                        -amplitude
                    };
                    value.to_le_bytes()
                })
                .collect()
        };
        let voiced = |pcm: &[u8]| super::voiced(pcm, voice::SAMPLE_RATE);
        assert!(!voiced(&[]), "an empty capture is not speech");
        // Steady hum just above the client's VAD floor, well under MIN_STT_RMS.
        assert!(!voiced(&samples(40, 4_800)));
        assert!(!voiced(&samples(0, 4_800)), "digital silence is not speech");
        assert!(!voiced(&[0, 4].repeat(4_800)), "DC offset is not speech");
        // Real speech energy passes.
        assert!(voiced(&samples(1_200, 4_800)));
    }

    #[tokio::test]
    async fn short_or_empty_utterance_clears_speaking_indicator() {
        use connectrpc::StreamMessage;
        use w::VoiceService;

        struct EmptyStt;
        impl SttEngine for EmptyStt {
            fn transcribe<'a>(
                &'a self,
                _: Vec<u8>,
                _: u32,
            ) -> voice::BoxFuture<'a, Result<String, String>> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                let store = app.world().resource::<Store>();
                store.register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.activate("ene", Some("c1"))?;
                app.world_mut().insert_resource(voice::Engines {
                    stt: Some(Arc::new(EmptyStt)),
                    ..Default::default()
                });
                Ok(())
            })
            .await
            .unwrap();
        let messages = vec![
            StreamMessage::from_message(&w::AudioInputChunk {
                client_id: "c1".into(),
                sample_rate: 24_000,
                pcm_data: vec![1; 400], // below MIN_STT_BYTES
                is_companion_speaking: false,
                ..Default::default()
            }),
            StreamMessage::from_message(&w::AudioInputChunk::default()),
            StreamMessage::from_message(&w::AudioInputChunk {
                client_id: "c1".into(),
                sample_rate: 24_000,
                pcm_data: vec![1; 4_000], // empty transcript from EmptyStt
                is_companion_speaking: false,
                ..Default::default()
            }),
            StreamMessage::from_message(&w::AudioInputChunk::default()),
        ];
        let service = Services {
            runtime: runtime.clone(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        };
        let response = service
            .stream_audio_input(
                RequestContext::new(http::HeaderMap::new()),
                Box::pin(futures_util::stream::iter(messages.into_iter().map(Ok))),
            )
            .await
            .unwrap();
        let events: Vec<w::TranscriptionEvent> = response
            .body
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(events.len(), 4);
        assert!(events[0].is_speaking);
        assert!(!events[1].is_speaking && !events[1].is_final);
        assert!(events[2].is_speaking);
        assert!(!events[3].is_speaking && !events[3].is_final);
        runtime.shutdown().await.unwrap();
    }
}
