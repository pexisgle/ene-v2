//! Voice service: microphone input (STT) and synthesized speech output (TTS).
//! The host owns generations, the audio-owning client and the speech floor
//! (docs/design/voice-avatar.md §4–§6). `StreamAudioInput` binds the stream to
//! the client that opened it and recognizes each finished utterance;
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
        let chunk = |client: &str, rate, sample: i16, speaking| {
            StreamMessage::from_message(&w::AudioInputChunk {
                client_id: client.into(),
                sample_rate: rate,
                pcm_data: sample.to_le_bytes().repeat(1_600),
                is_companion_speaking: speaking,
                ..Default::default()
            })
        };
        let messages = vec![
            chunk("c1", 24_000, 1, false),
            // A frame that names another client never joins this stream.
            chunk("other", 24_000, 2, false),
            chunk("c1", 24_000, 3, false),
            // Playback echo discards the capture in progress.
            chunk("", 0, 4, true),
            chunk("", 16_000, 5, false),
            chunk("", 24_000, 6, false),
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
        assert_eq!(
            *captured.lock().unwrap(),
            [(16_000, vec![5; 1_600]), (24_000, vec![6; 1_600])]
        );
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
}
