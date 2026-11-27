//! Voice service: microphone input (STT) and synthesized speech output (TTS).
//! The host owns generations, the audio-owning client and the speech floor
//! (docs/design/client.md). `StreamAudioInput` binds the stream to
//! the client that opened it and recognizes each finished utterance;
//! `SubscribeAudioOutput` only pumps the host's generation-gated channel.
use super::voice_convert::to_wire_event;
use super::{Services, parse_client_id};
use crate::host::{
    Runtime,
    error::HostError,
    voice::{self, SetupError, SttEngine},
};
use connectrpc::{
    ConnectError, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};
use ene_domain::ClientId;
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
            let mut recognition: Option<Recognition> = None;
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
                            let chunk = message.view();
                            let current = match &mut recognition {
                                Some(current) => current,
                                slot @ None => match bind_recognition_client(&runtime, chunk.client_id).await {
                                    Ok(client) => slot.insert(Recognition { client, sample_rate: voice::SAMPLE_RATE, capture: SpeechCapture::Idle }),
                                    Err(error) => {
                                        yield Err(error);
                                        break;
                                    }
                                },
                            };
                            if !chunk.client_id.is_empty() && chunk.client_id != current.client.as_str() {
                                continue;
                            }
                            if chunk.barge_in {
                                flush_barge_in(&runtime, current.client.clone()).await;
                            } else if chunk.is_companion_speaking {
                                current.capture = SpeechCapture::Idle;
                                continue;
                            }
                            if chunk.sample_rate != 0 {
                                match validate_and_apply_sample_rate(current, &runtime, &engine, chunk.sample_rate).await {
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
                                let mut pcm = chunk.pcm_data;
                                while !pcm.is_empty() {
                                    let (prefix, rest) = pcm.split_at(pcm.len().min(current.capture.remaining()));
                                    pcm = rest;
                                    if let Some(started) = current.capture.append(prefix) {
                                        yield Ok(started);
                                    }
                                    if current.capture.remaining() != 0 {
                                        continue;
                                    }
                                    if let Some(event) = current.flush(&runtime, &engine).await? {
                                        yield Ok(event);
                                    }
                                }
                                (false, false)
                            }
                        }
                    };
                let event = match (flush_after, &mut recognition) {
                    (true, Some(recognition)) => recognition.flush(&runtime, &engine).await?,
                    _ => None,
                };
                if let Some(event) = event {
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
            .inspect(|services| Ok(voice::tts_source(&services.voice_context(), None)))
            .await
            .map_err(ConnectError::from)?
        {
            return Err(setup_error(error));
        }
        let client_id = parse_client_id(request.client_id)?;
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

    async fn speak(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SpeakRequest>,
    ) -> ServiceResult<w::Empty> {
        tracing::Span::current().record("companion_id", request.companion_id.unwrap_or(""));
        tracing::Span::current().record("client_id", request.client_id);
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
        let (source, voice_name, companion, subscription) = self
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
                    require_active_output(store, &companion, &client_id)?;
                    let subscription = services
                        .voice
                        .output_subscription(&companion, &client_id)
                        .ok_or_else(|| {
                        HostError::FailedPrecondition(
                            "the client has no audio subscription for this companion".into(),
                        )
                    })?;
                    let binding = store
                        .companion_repo()
                        .companion(&companion)?
                        .as_ref()
                        .map(voice::parse_binding);
                    let source = voice::tts_source(&services.voice_context(), binding.as_ref())
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
                    let voice = binding
                        .and_then(|binding| binding.voice_id)
                        .unwrap_or_default();
                    Ok((source, voice, companion, subscription))
                }
            })
            .await
            .map_err(ConnectError::from)?;
        let (pcm, rate) = voice::synthesize_for_probe(source, &text, voice_name)
            .await
            .map_err(|error| ConnectError::unavailable(error.to_string()))?;
        self.runtime
            .inspect(move |services| {
                require_active_output(&services.store, &companion, &client_id)?;
                voice::play_once(
                    &services.voice,
                    &companion,
                    &client_id,
                    &subscription,
                    pcm,
                    rate,
                )
            })
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::Empty::default())
    }
}

fn require_active_output(
    store: &crate::host::db::Store,
    companion: &str,
    client: &str,
) -> crate::host::db::Result<()> {
    if store
        .client_repo()
        .activation(companion)?
        .and_then(|owner| owner.client_id)
        .as_deref()
        != Some(client)
    {
        return Err(HostError::FailedPrecondition(
            "the client no longer owns this companion's audio output".into(),
        ));
    }
    Ok(())
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
enum SpeechCapture {
    #[default]
    Idle,
    Speaking {
        id: String,
        pcm: Vec<u8>,
    },
}

impl SpeechCapture {
    fn append(&mut self, chunk: &[u8]) -> Option<w::TranscriptionEvent> {
        match self {
            Self::Speaking { pcm, .. } => {
                pcm.extend_from_slice(chunk);
                None
            }
            Self::Idle => {
                let id = uuid::Uuid::new_v4().to_string();
                let event = w::TranscriptionEvent {
                    transcription_id: id.clone(),
                    is_speaking: true,
                    ..Default::default()
                };
                *self = Self::Speaking {
                    id,
                    pcm: chunk.to_vec(),
                };
                Some(event)
            }
        }
    }

    fn remaining(&self) -> usize {
        match self {
            Self::Idle => voice::MAX_STT_BYTES,
            Self::Speaking { pcm, .. } => voice::MAX_STT_BYTES - pcm.len(),
        }
    }
}

struct Recognition {
    client: ClientId,
    sample_rate: u32,
    capture: SpeechCapture,
}

impl Recognition {
    async fn flush(
        &mut self,
        runtime: &Runtime,
        engine: &Arc<dyn SttEngine>,
    ) -> Result<Option<w::TranscriptionEvent>, ConnectError> {
        let SpeechCapture::Speaking { id, pcm } = std::mem::take(&mut self.capture) else {
            return Ok(None);
        };
        let stopped = |id| {
            Some(w::TranscriptionEvent {
                transcription_id: id,
                ..Default::default()
            })
        };
        if pcm.len() < voice::MIN_STT_BYTES || !voiced(&pcm, self.sample_rate) {
            return Ok(stopped(id));
        }
        let client = self.client.clone();
        let active = runtime
            .inspect(move |services| {
                Ok(services
                    .store
                    .client_repo()
                    .activation_for_client(client.as_str())?
                    .is_some())
            })
            .await
            .map_err(ConnectError::from)?;
        if !active {
            return Ok(stopped(id));
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
            return Ok(stopped(id));
        }
        let accepted = runtime
            .call_kernel({
                let id = id.clone();
                let text = text.clone();
                let client = self.client.clone();
                move |kernel| kernel.accept_transcription(&id, &text, &client)
            })
            .await
            .map_err(ConnectError::from)?;
        if !accepted {
            return Ok(stopped(id));
        }
        Ok(Some(w::TranscriptionEvent {
            transcription_id: id,
            text,
            is_final: true,
            ..Default::default()
        }))
    }
}

async fn bind_recognition_client(
    runtime: &Runtime,
    client_id: &str,
) -> Result<ClientId, ConnectError> {
    let client = parse_client_id(client_id)?;
    runtime
        .inspect(move |services| {
            voice::active_companions(&services.store, &client)?;
            Ok(client)
        })
        .await
        .map_err(ConnectError::from)
}

async fn flush_barge_in(runtime: &Runtime, client: ClientId) {
    let _written = runtime
        .change(move |services| {
            let companions = services
                .store
                .client_repo()
                .active_companions_for_client(client.as_str())?;
            for companion in companions {
                let _flushed =
                    voice::flush(Some(&services.voice), &companion, w::FlushReason::BargeIn);
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
    let flushed = if sample_rate != recognition.sample_rate {
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
    client_id: ClientId,
    subscription: String,
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.voice
            .unregister(self.client_id.as_str(), &self.subscription);
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

    // Alternating amplitude reaches the recognizer without a DC offset.
    fn alternation(amplitude: i16, samples: usize) -> Vec<i16> {
        [amplitude, -amplitude]
            .into_iter()
            .cycle()
            .take(samples)
            .collect()
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
        const SAMPLES: usize = voice::MIN_STT_BYTES / 2;
        let wave = |amplitude| alternation(amplitude, SAMPLES);
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
        let mut messages = vec![
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
        let max_samples = voice::MAX_STT_BYTES / 2;
        let first = wave(6_000);
        let tail = wave(7_000);
        let mut longer = first.repeat(max_samples / SAMPLES);
        longer.truncate(max_samples - SAMPLES / 2);
        longer.extend(tail.iter().cycle().take(SAMPLES + SAMPLES / 2));
        // With the first frame half-sized, regular frames straddle the limit.
        messages.push(chunk("", 0, longer[..SAMPLES / 2].to_vec(), false));
        messages.extend(
            longer[SAMPLES / 2..]
                .chunks(SAMPLES)
                .map(|pcm| chunk("", 0, pcm.to_vec(), false)),
        );
        let service = Services::new(runtime.clone(), tokio_util::sync::CancellationToken::new());
        let response = service
            .stream_audio_input(
                RequestContext::new(http::HeaderMap::new()),
                Box::pin(futures_util::stream::iter(messages.into_iter().map(Ok))),
            )
            .await
            .unwrap();
        let events: Vec<_> = response.body.map(Result::unwrap).collect().await;
        for (index, event) in events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.is_final)
        {
            assert!(
                events[..index].iter().any(|started| started.is_speaking
                    && started.transcription_id == event.transcription_id)
            );
        }
        let finals: Vec<_> = events
            .into_iter()
            .filter(|event| event.is_final)
            .map(|event| event.text)
            .collect();
        assert_eq!(finals, ["capture 1", "capture 2", "capture 3", "capture 4"]);
        {
            let captured = captured.lock();
            assert_eq!(
                captured
                    .iter()
                    .map(|(rate, samples)| (*rate, samples.len()))
                    .collect::<Vec<_>>(),
                [
                    (16_000, SAMPLES),
                    (24_000, quiet.len()),
                    (24_000, max_samples),
                    (24_000, SAMPLES),
                ]
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
            assert_eq!(captured[2].1, longer[..max_samples]);
            assert_eq!(captured[3].1, longer[max_samples..]);
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
        let first = ClientId::parse("c1").unwrap();
        let old = runtime
            .inspect(move |services| {
                voice::subscribe_audio(&services.store, &services.voice, &first)
            })
            .await
            .unwrap();
        let old_guard = StreamGuard {
            voice: runtime
                .inspect(|services| Ok(services.voice.clone()))
                .await
                .unwrap(),
            client_id: ClientId::parse("c1").unwrap(),
            subscription: old.subscription_id,
        };
        let second = ClientId::parse("c1").unwrap();
        let mut current = runtime
            .inspect(move |services| {
                voice::subscribe_audio(&services.store, &services.voice, &second)
            })
            .await
            .unwrap();
        let current_guard = StreamGuard {
            voice: runtime
                .inspect(|services| Ok(services.voice.clone()))
                .await
                .unwrap(),
            client_id: ClientId::parse("c1").unwrap(),
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
        use std::sync::atomic::{AtomicUsize, Ordering};
        use w::VoiceService;

        #[derive(Default)]
        struct EmptyStt(AtomicUsize);
        impl SttEngine for EmptyStt {
            fn transcribe<'a>(
                &'a self,
                _: Vec<u8>,
                _: u32,
            ) -> voice::BoxFuture<'a, voice::HttpResult<String>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(String::new()) })
            }
        }

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let recognizer = Arc::new(EmptyStt::default());
        let installed = Arc::clone(&recognizer);
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
                services.engine_overrides.write().stt = Some(installed);
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
                pcm_data: [0xb0, 0x04, 0x50, 0xfb].repeat(2_400),
                is_companion_speaking: false,
                ..Default::default()
            }),
            StreamMessage::from_message(&w::AudioInputChunk::default()),
        ];
        let service = Services::new(runtime.clone(), tokio_util::sync::CancellationToken::new());
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
        assert_eq!(recognizer.0.load(Ordering::SeqCst), 1);
        runtime.shutdown().await.unwrap();
    }
}
