//! Voice service: microphone input (STT) and synthesized speech output (TTS).
//! The host owns generations, the audio-owning client and the speech floor
//! (docs/design/voice-avatar.md §4–§6). `StreamAudioInput` binds the stream to
//! the client that opened it and recognizes each finished utterance;
//! `SubscribeAudioOutput` only pumps the host's generation-gated channel.
use super::Services;
use super::voice_convert::to_wire_event;
use crate::host::{
    Runtime,
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

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::VoiceService for Services {
    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "stream_audio_input",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
        )
    )]
    async fn stream_audio_input(
        &self,
        _: RequestContext,
        mut inbound: InboundStream<w::AudioInputChunk>,
    ) -> ServiceResult<ServiceStream<w::TranscriptionEvent>> {
        let engine = match self
            .runtime
            .inspect(|services| Ok(voice::stt_source(&services.voice_context())))
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
                                let client_id = message.view().client_id.to_owned();
                                match bind_recognition_client(&runtime, client_id).await {
                                    Ok(binding) => recognition.binding = Some(binding),
                                    Err(error) => {
                                        yield Err(error);
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
                            if chunk.barge_in {
                                if let Some(binding) = recognition.binding.as_ref() {
                                    flush_barge_in(&runtime, binding.client_id.clone()).await;
                                }
                            } else if chunk.is_companion_speaking {
                                recognition.pending.clear();
                                recognition.speaking = false;
                                recognition.transcription_id.clear();
                                continue;
                            }
                            if chunk.sample_rate != 0 {
                                match validate_and_apply_sample_rate(&mut recognition, &runtime, &engine, chunk.sample_rate).await {
                                    Ok(Some(event)) => yield Ok(event),
                                    Ok(None) => {}
                                    Err(error) => {
                                        yield Err(error);
                                        break;
                                    }
                                }
                            }
                            if chunk.pcm_data.is_empty() {
                                (true, false)
                            } else {
                                if let Some(started) = recognition.start_speaking_if_needed() {
                                    yield Ok(started);
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "subscribe_audio_output",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            client_id = %request.client_id,
        )
    )]
    async fn subscribe_audio_output(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeAudioRequest>,
    ) -> ServiceResult<ServiceStream<w::AudioOutputEvent>> {
        if let Err(error) = self
            .runtime
            .inspect(|services| Ok(voice::tts_source(&services.voice_context())))
            .await
            .map_err(ConnectError::from)?
        {
            return Err(setup_error(error));
        }
        let client_id = request.client_id.to_owned();
        let (mut receiver, mut control, guard) = self
            .runtime
            .inspect(move |services| {
                let voice::AudioSubscription {
                    subscription_id,
                    receiver,
                    control,
                } = voice::subscribe_audio(&services.store, &services.voice, &client_id)?;
                let guard = StreamGuard {
                    voice: services.voice.clone(),
                    client_id,
                    subscription: subscription_id,
                };
                Ok((receiver, control, guard))
            })
            .await
            .map_err(ConnectError::from)?;
        let stream = async_stream::stream! {
            loop {
                let event = tokio::select! {
                    biased;
                    event = control.recv() => event,
                    event = receiver.recv() => event,
                };
                let Some(event) = event else { break };
                yield Ok(to_wire_event(event));
            }
            drop(guard);
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "speak",
            companion_id = request.companion_id.unwrap_or(""),
            request_id = tracing::field::Empty,
            client_id = %request.client_id,
        )
    )]
    async fn speak(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SpeakRequest>,
    ) -> ServiceResult<w::Empty> {
        let client_id = request.client_id.to_owned();
        let requested_companion = request
            .companion_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
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
        let (source, voice_name, companion) = self
            .runtime
            .inspect({
                let client_id = client_id.clone();
                let requested_companion = requested_companion.clone();
                move |services| {
                    let store = &services.store;
                    let companion = match requested_companion {
                        Some(id) => id,
                        None => store
                            .client_repo()
                            .activation_for_client(&client_id)?
                            .ok_or_else(|| {
                                HostError::FailedPrecondition(
                                    "the client is not the active client of any companion".into(),
                                )
                            })?,
                    };
                    let companion_record = store.companion_repo().companion(&companion)?;
                    let source = voice::companion_tts_source(
                        &services.voice_context(),
                        companion_record.as_ref(),
                    )
                    .map_err(|error| match error {
                        SetupError::Missing => {
                            HostError::FailedPrecondition(voice::VOICE_UNAVAILABLE.into())
                        }
                        SetupError::Disabled(message)
                        | SetupError::Broken(message)
                        | SetupError::Unsupported(message) => {
                            HostError::FailedPrecondition(message)
                        }
                    })?;
                    let voice = companion_record
                        .as_ref()
                        .and_then(|record| voice::parse_binding(record).voice_id)
                        .unwrap_or_default();
                    Ok((source, voice, companion))
                }
            })
            .await
            .map_err(ConnectError::from)?;
        let (pcm, rate) = voice::synthesize_for_probe(source, &text, voice_name)
            .await
            .map_err(|error| ConnectError::unavailable(error.to_string()))?;
        self.runtime
            .inspect(move |services| voice::play_once(&services.voice, &companion, pcm, rate))
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::Empty::default())
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
    fn start_speaking_if_needed(&mut self) -> Option<w::TranscriptionEvent> {
        if self.speaking {
            return None;
        }
        self.speaking = true;
        if self.transcription_id.is_empty() {
            self.transcription_id = uuid::Uuid::new_v4().to_string();
        }
        Some(w::TranscriptionEvent {
            transcription_id: self.transcription_id.clone(),
            is_speaking: true,
            ..Default::default()
        })
    }

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
            .inspect({
                let binding = binding.clone();
                move |services| voice::is_still_active(&services.store, &binding)
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
            .call_kernel({
                let id = transcription_id.clone();
                let text = text.clone();
                move |kernel| kernel.accept_transcription(&id, &text, &binding)
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

async fn bind_recognition_client(
    runtime: &Runtime,
    client_id: String,
) -> std::result::Result<InputBinding, ConnectError> {
    runtime
        .inspect(move |services| voice::bind_input(&services.store, &client_id))
        .await
        .map_err(ConnectError::from)
}

async fn flush_barge_in(runtime: &Runtime, client_id: String) {
    let _written = runtime
        .change(move |services| {
            let companions = services
                .store
                .client_repo()
                .active_companions_for_client(&client_id)
                .unwrap_or_default();
            let voice = Some(&services.voice);
            for companion_id in companions {
                let _flushed = voice::flush(voice, &companion_id, w::FlushReason::BargeIn);
            }
            Ok(())
        })
        .await;
}

async fn validate_and_apply_sample_rate(
    recognition: &mut Recognition,
    runtime: &Runtime,
    engine: &Arc<dyn SttEngine>,
    sample_rate: u32,
) -> std::result::Result<Option<w::TranscriptionEvent>, ConnectError> {
    if !voice::STT_SAMPLE_RATES.contains(&sample_rate) {
        return Err(ConnectError::invalid_argument(
            "audio must be 16 kHz or 24 kHz mono PCM",
        ));
    }
    let flushed = if sample_rate != recognition.sample_rate && !recognition.pending.is_empty() {
        recognition.flush(runtime, engine).await?
    } else {
        None
    };
    recognition.sample_rate = sample_rate;
    Ok(flushed)
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
    voice: voice::Voice,
    client_id: String,
    subscription: String,
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.voice.unregister(&self.client_id, &self.subscription);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::{Store, clients::ClientDeclaration};

    use rig_ecs::prelude::Update;
    use tokio::sync::mpsc::error::TryRecvError;

    type Captured = Arc<parking_lot::Mutex<Vec<(u32, Vec<i16>)>>>;

    struct RecordingStt(Captured);

    impl SttEngine for RecordingStt {
        fn transcribe<'a>(
            &'a self,
            pcm: Vec<u8>,
            sample_rate: u32,
        ) -> voice::BoxFuture<'a, voice::HttpResult<String>> {
            Box::pin(async move {
                let samples = pcm
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect();
                let mut received = self.0.lock();
                received.push((sample_rate, samples));
                Ok(format!("capture {}", received.len()))
            })
        }
    }

    #[tokio::test]
    async fn microphone_stream_keeps_binding_echo_and_rate_boundaries() {
        use connectrpc::StreamMessage;
        use w::VoiceService;

        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recording = RecordingStt(captured.clone());
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .inspect(move |services| {
                let store = &services.store;
                store.client_repo().register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.client_repo().activate("ene", Some("c1"))?;
                services.engine_overrides.write().stt = Some(Arc::new(recording));
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
            let captured = captured.lock();
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

    /// Counts kernel scheduler passes. Registered on the world as a plain system
    /// over a global counter, so the transport tests never declare an ECS resource.
    static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn frames() -> u64 {
        FRAMES.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn audio_guard_cleanup_outside_tokio_preserves_replacement_without_a_frame() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .read(|app| {
                app.add_systems(Update, || {
                    FRAMES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                });
                let store = app.world().resource::<Store>();
                store.client_repo().register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.client_repo().activate("ene", Some("c1"))?;
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
            .inspect(move |services| {
                voice::subscribe_audio(&services.store, &services.voice, &first.client_id)
            })
            .await
            .unwrap();
        let old_guard = StreamGuard {
            voice: runtime
                .inspect(|services| Ok(services.voice.clone()))
                .await
                .unwrap(),
            client_id: "c1".into(),
            subscription: old.subscription_id,
        };
        let second = request();
        let mut current = runtime
            .inspect(move |services| {
                voice::subscribe_audio(&services.store, &services.voice, &second.client_id)
            })
            .await
            .unwrap();
        let current_guard = StreamGuard {
            voice: runtime
                .inspect(|services| Ok(services.voice.clone()))
                .await
                .unwrap(),
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
            .read(move |_| {
                let before = frames();
                drop(current_guard);
                let after = check.read(|_| Ok(frames()));
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
        assert!(runtime.read(|_| Ok(frames())).await.unwrap() > before);
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
            ) -> voice::BoxFuture<'a, voice::HttpResult<String>> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .inspect(|services| {
                let store = &services.store;
                store.client_repo().register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })?;
                store.client_repo().activate("ene", Some("c1"))?;
                services.engine_overrides.write().stt = Some(Arc::new(EmptyStt));
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
