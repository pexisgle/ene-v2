//! The audio and capture half of the serve loop: the stream aliases, the frame
//! futures, the capture tick, and the microphone retry for [`Session`].
//!
//! These are free functions rather than methods because the serve loop keeps
//! its state in one place: each takes the session it acts on.

use super::{Outcome, Session, persistent, stream::StreamLoopState};
use crate::{
    audio::{self, Microphone, Player},
    host::Host,
    state::Device,
    update::Update,
};
use ene_rpc as w;
use std::time::Duration;

pub(super) type AudioOutStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::AudioOutputEventView<'static>,
>;
pub(super) type MicSendHalf = connectrpc::client::BidiSendHalf<w::AudioInputChunk>;
pub(super) type MicRecvHalf = connectrpc::client::BidiRecvHalf<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::TranscriptionEventView<'static>,
>;
pub(super) async fn open_audio_output_stream(
    session: &mut Session,
    host: &Host,
) -> Option<AudioOutStream> {
    if session.transport.bindings.is_empty() {
        return None;
    }
    match host
        .voice
        .subscribe_audio_output(w::SubscribeAudioRequest {
            client_id: session.transport.options.id.clone(),
            ..Default::default()
        })
        .await
    {
        Ok(stream) => {
            if let Some(player) = session.audio.player.as_mut() {
                player.reset_stream();
            }
            Some(stream)
        }
        Err(error) => {
            tracing::info!("no audio output stream: {error}");
            session
                .set_out(Device::Unavailable(error.to_string()))
                .await;
            None
        }
    }
}

pub(super) async fn open_audio_input_stream(
    session: &mut Session,
    host: &Host,
) -> (Option<MicSendHalf>, Option<MicRecvHalf>) {
    if session.transport.bindings.is_empty() {
        return (None, None);
    }
    session.open_microphone().await;
    match host.voice.stream_audio_input().await {
        Ok(stream) => {
            let (send, recv) = stream.into_split();
            (Some(send), Some(recv))
        }
        Err(error) => {
            tracing::info!("no microphone stream: {error}");
            (None, None)
        }
    }
}

pub(super) async fn handle_audio_frame(
    session: &mut Session,
    state: &mut StreamLoopState,
    frame: Option<Result<w::audio_output_event::Event, connectrpc::ConnectError>>,
) -> Option<Outcome> {
    match frame {
        Some(Ok(event)) => {
            session.audio.play(event);
            None
        }
        Some(Err(error)) => {
            // A host without TTS answers `unimplemented` or
            // `failed_precondition`; that is a client without audio,
            // not a broken link.
            if persistent(&error) {
                tracing::info!("the host has no audio output: {error}");
                session
                    .set_out(Device::Unavailable(error.to_string()))
                    .await;
                state.output = None;
                None
            } else {
                Some(session.transport.classify(error))
            }
        }
        None => {
            state.output = None;
            None
        }
    }
}

pub(super) fn handle_transcription_frame(
    session: &mut Session,
    state: &mut StreamLoopState,
    frame: Option<Result<w::TranscriptionEvent, connectrpc::ConnectError>>,
) {
    match frame {
        Some(Ok(event)) => {
            state.mic_error_reported = false;
            super::notify_ui(
                &session.updates,
                Update::Transcription {
                    text: event.text,
                    is_final: event.is_final,
                    is_speaking: event.is_speaking,
                },
            );
        }
        Some(Err(error)) => {
            super::notify_ui(
                &session.updates,
                Update::Transcription {
                    text: String::new(),
                    is_final: true,
                    is_speaking: false,
                },
            );
            if persistent(&error) {
                tracing::info!("the host has no speech recognition: {error}");
                state.mic_retry_at = None;
            } else {
                tracing::warn!("the transcription stream ended: {error}");
                if !state.mic_error_reported {
                    state.mic_error_reported = true;
                    super::notify_ui(
                        &session.updates,
                        Update::Failed {
                            context: "音声認識",
                            message: error.to_string(),
                        },
                    );
                }
                if !session.transport.bindings.is_empty() && session.audio.microphone.is_some() {
                    state.mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
            state.mic_recv = None;
            state.mic_send = None;
        }
        None => {
            super::notify_ui(
                &session.updates,
                Update::Transcription {
                    text: String::new(),
                    is_final: true,
                    is_speaking: false,
                },
            );
            if !session.transport.bindings.is_empty() && session.audio.microphone.is_some() {
                state.mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            state.mic_recv = None;
            state.mic_send = None;
        }
    }
}

pub(super) async fn handle_mic_retry(
    session: &mut Session,
    host: &Host,
    state: &mut StreamLoopState,
) {
    state.mic_retry_at = None;
    if session.transport.bindings.is_empty() || state.mic_recv.is_some() {
        return;
    }
    match host.voice.stream_audio_input().await {
        Ok(stream) => {
            let (send, recv) = stream.into_split();
            state.mic_send = Some(send);
            state.mic_recv = Some(recv);
            state.mic_error_reported = false;
        }
        Err(error) => {
            if !persistent(&error) {
                state.mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(10));
            }
        }
    }
}

pub(super) async fn handle_capture_tick(session: &mut Session, state: &mut StreamLoopState) {
    let speaking = session
        .audio
        .player
        .as_ref()
        .is_some_and(|player| player.playing() && !player.is_muted());
    let far_end_level = session.audio.player.as_ref().map_or(0., Player::level);
    let CaptureTick {
        chunks,
        state: capture_state,
        barge_in,
    } = capture_tick(
        &mut session.audio.microphone,
        !session.transport.bindings.is_empty(),
        &session.transport.options.id,
        speaking,
        far_end_level,
    );
    if barge_in && let Some(player) = session.audio.player.as_mut() {
        player.stop_current();
        session.audio.speaking_companion = None;
    }
    let mut sent = 0;
    if let Some(stream) = state.mic_send.as_mut() {
        for chunk in chunks {
            let bytes = chunk.pcm_data.len();
            if let Err(error) = stream.send(chunk).await {
                tracing::info!("the microphone stream closed: {error}");
                state.mic_send = None;
                break;
            }
            sent += bytes;
        }
    }
    super::notify_ui(
        &session.updates,
        Update::Capture {
            level: capture_state.level,
            open: capture_state.open,
            sent,
        },
    );
}

/// One frame from the audio stream. An absent stream parks this branch forever
/// instead of returning at once, which would spin a core.
pub(super) async fn audio_message(
    output: &mut Option<AudioOutStream>,
) -> Option<Result<w::audio_output_event::Event, connectrpc::ConnectError>> {
    let Some(stream) = output.as_mut() else {
        return std::future::pending().await;
    };
    match stream.message().await {
        Ok(Some(message)) => Some(
            message
                .to_owned_message()
                .event
                .ok_or_else(|| connectrpc::ConnectError::internal("an empty audio event")),
        ),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// One live transcription event from the bidirectional voice input stream.
pub(super) async fn transcription_message(
    input: &mut Option<MicRecvHalf>,
) -> Option<Result<w::TranscriptionEvent, connectrpc::ConnectError>> {
    let Some(stream) = input.as_mut() else {
        return std::future::pending().await;
    };
    match stream.message::<w::TranscriptionEvent>().await {
        Ok(Some(message)) => Some(Ok(message.to_owned_message())),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

pub(super) async fn retry_wait(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// What one capture tick produced, and what the settings probe displays.
struct CaptureTick {
    chunks: Vec<w::AudioInputChunk>,
    state: CaptureState,
    barge_in: bool,
}

#[derive(Clone, Copy, Default)]
struct CaptureState {
    level: f32,
    open: bool,
}

/// Takes ordered PCM chunks and end markers from the microphone.
fn capture_tick(
    microphone: &mut Option<Microphone>,
    active: bool,
    client_id: &str,
    speaking: bool,
    far_end_level: f32,
) -> CaptureTick {
    let Some(microphone) = microphone.as_mut() else {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState::default(),
            barge_in: false,
        };
    };
    // The level is read before the gate so the probe shows the microphone even
    // while the gate is closed; that is the difference the test screen exists to
    // make visible.
    let level = microphone.level();
    if !active {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState { level, open: false },
            barge_in: false,
        };
    }
    let output = microphone.take(speaking, far_end_level);
    let state = CaptureState {
        level,
        open: output.open,
    };
    let barge_in = output.barge_in;
    let mut chunks: Vec<w::AudioInputChunk> = output
        .chunks
        .into_iter()
        .enumerate()
        .map(|(index, pcm_data)| w::AudioInputChunk {
            client_id: client_id.to_owned(),
            pcm_data,
            sample_rate: audio::WIRE_RATE,
            is_companion_speaking: speaking && !barge_in,
            barge_in: barge_in && index == 0,
            ..Default::default()
        })
        .collect();
    if barge_in && chunks.is_empty() {
        chunks.push(w::AudioInputChunk {
            client_id: client_id.to_owned(),
            sample_rate: audio::WIRE_RATE,
            barge_in: true,
            ..Default::default()
        });
    }
    CaptureTick {
        chunks,
        state,
        barge_in,
    }
}
