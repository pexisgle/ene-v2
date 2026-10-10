//! A bounded protobuf WebSocket transport. Speech engines plug in here; the
//! transport never pretends that microphone data alone implements STT/TTS.
use crate::api::{
    auth::{self, Principal},
    state::App,
};
use axum::{
    extract::{
        Extension, State, WebSocketUpgrade,
        ws::{Message as Frame, WebSocket},
    },
    response::Response,
};
use buffa::Message;
use ene_api_types::rpc as w;
use futures_util::SinkExt;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::task::TaskTracker;
use uuid::Uuid;

type EngineFuture = Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>;
pub(crate) trait Engine: Send + Sync {
    fn run(
        &self,
        start: w::AudioStart,
        input: mpsc::Receiver<w::AudioChunk>,
        output: mpsc::Sender<w::AudioFrame>,
        cancelled: watch::Receiver<bool>,
    ) -> EngineFuture;
}
#[derive(Clone, Default)]
pub(crate) struct Audio {
    state: Arc<Mutex<Sessions>>,
    engine: Option<Arc<dyn Engine>>,
    tasks: TaskTracker,
}
#[derive(Default)]
struct Sessions {
    generations: HashMap<String, u64>,
    prepared: HashMap<String, Prepared>,
    live: HashMap<String, Live>,
}
struct Prepared {
    start: w::AudioStart,
    expires: tokio::time::Instant,
    permission: watch::Receiver<()>,
}
struct Live {
    companion: String,
    lease: Weak<()>,
    cancel: watch::Sender<bool>,
}
impl Audio {
    pub fn available(&self) -> bool {
        self.engine.is_some()
    }
    #[cfg(test)]
    pub fn with_engine(engine: Arc<dyn Engine>) -> Self {
        Self {
            engine: Some(engine),
            ..Default::default()
        }
    }
    pub async fn prepare(
        &self,
        request: w::PrepareAudioRequest,
        permission: watch::Receiver<()>,
    ) -> Result<w::AudioStart, connectrpc::ConnectError> {
        if !self.available() {
            return Err(connectrpc::ConnectError::unimplemented(
                "音声エンジンが接続されていません",
            ));
        }
        if request.codec != "pcm_s16le"
            || !matches!(request.sample_rate, 16_000 | 24_000 | 48_000)
            || request.channels != 1
        {
            return Err(connectrpc::ConnectError::invalid_argument(
                "未対応の音声形式です",
            ));
        }
        let mut state = self.state.lock().await;
        let now = tokio::time::Instant::now();
        state.prepared.retain(|_, ticket| {
            ticket.expires > now
                && ticket
                    .permission
                    .has_changed()
                    .is_ok_and(|changed| !changed)
        });
        if state.prepared.len() >= 128 {
            return Err(connectrpc::ConnectError::resource_exhausted(
                "音声の準備数の上限です",
            ));
        }
        let generation = *state
            .generations
            .entry(request.companion_id.clone())
            .or_default();
        if generation == u64::MAX {
            return Err(connectrpc::ConnectError::resource_exhausted(
                "音声世代の上限です",
            ));
        }
        let start = w::AudioStart {
            session_id: Uuid::new_v4().to_string(),
            generation,
            companion_id: request.companion_id,
            device_id: request.device_id,
            codec: request.codec,
            sample_rate: request.sample_rate,
            channels: request.channels,
            ..Default::default()
        };
        state.prepared.insert(
            start.session_id.clone(),
            Prepared {
                start: start.clone(),
                expires: now + Duration::from_secs(30),
                permission,
            },
        );
        Ok(start)
    }
    pub async fn stop(&self, companion: &str) {
        let mut state = self.state.lock().await;
        if let Some(generation) = state.generations.get_mut(companion) {
            *generation = generation.saturating_add(1);
        }
        state
            .prepared
            .retain(|_, ticket| ticket.start.companion_id != companion);
        for session in state.live.values().filter(|s| s.companion == companion) {
            session.cancel.send_replace(true);
        }
    }
    pub async fn shutdown(&self) {
        for session in self.state.lock().await.live.values() {
            session.cancel.send_replace(true);
        }
        self.tasks.close();
        self.tasks.wait().await;
    }
    async fn reserve(
        &self,
        start: &w::AudioStart,
    ) -> Result<(Arc<()>, watch::Receiver<bool>), &'static str> {
        if Uuid::parse_str(&start.session_id).is_err()
            || start.codec != "pcm_s16le"
            || !matches!(start.sample_rate, 16_000 | 24_000 | 48_000)
            || start.channels != 1
        {
            return Err("invalid_audio_format");
        }
        let mut state = self.state.lock().await;
        state.live.retain(|_, s| s.lease.strong_count() > 0);
        let Some(ticket) = state.prepared.get(&start.session_id) else {
            return Err("invalid_audio_session");
        };
        if ticket.start != *start
            || ticket.expires <= tokio::time::Instant::now()
            || !ticket
                .permission
                .has_changed()
                .is_ok_and(|changed| !changed)
        {
            return Err("invalid_audio_session");
        }
        if state
            .generations
            .get(&start.companion_id)
            .copied()
            .unwrap_or(0)
            == u64::MAX
            || state
                .generations
                .get(&start.companion_id)
                .copied()
                .unwrap_or(0)
                != start.generation
        {
            return Err("cancelled");
        }
        if state.live.len() >= 8 {
            return Err("resource_limit");
        }
        if state
            .live
            .values()
            .any(|s| s.companion == start.companion_id && !*s.cancel.borrow())
        {
            return Err("state_conflict");
        }
        let lease = Arc::new(());
        let (cancel, cancelled) = watch::channel(false);
        state.prepared.remove(&start.session_id);
        state.live.insert(
            start.session_id.clone(),
            Live {
                companion: start.companion_id.clone(),
                lease: Arc::downgrade(&lease),
                cancel,
            },
        );
        Ok((lease, cancelled))
    }
}
pub(crate) async fn upgrade(
    State(app): State<App>,
    Extension(principal): Extension<Principal>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| stream(app, principal, socket))
}
async fn send(socket: &mut WebSocket, frame: w::AudioFrame) -> bool {
    if frame.encoded_len() > 64 * 1024 {
        return false;
    }
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(Frame::Binary(frame.encode_to_vec().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn fail(socket: &mut WebSocket, id: &str, code: &str) {
    let _ = send(
        socket,
        w::AudioFrame {
            frame: Some(w::audio_frame::Frame::Failure(Box::new(w::AudioFailure {
                session_id: id.into(),
                code: code.into(),
                ..Default::default()
            }))),
            ..Default::default()
        },
    )
    .await;
}
async fn stream(app: App, principal: Principal, mut socket: WebSocket) {
    let incoming = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let Ok(Some(Ok(Frame::Binary(bytes)))) = incoming else {
        return;
    };
    let Ok(frame) = w::AudioFrame::decode_from_slice(&bytes) else {
        fail(&mut socket, "", "invalid_request").await;
        return;
    };
    let Some(w::audio_frame::Frame::Start(start)) = frame.frame else {
        fail(&mut socket, "", "invalid_request").await;
        return;
    };
    if matches!(&principal,Principal::Device(id, _) if *id!=start.device_id) {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    if app
        .devices
        .permitted(&start.device_id, "audio.input")
        .await
        .is_err()
        || app
            .devices
            .permitted(&start.device_id, "audio.output")
            .await
            .is_err()
    {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    if app
        .application
        .companion(&start.companion_id)
        .await
        .is_err()
    {
        fail(&mut socket, &start.session_id, "not_found").await;
        return;
    }
    let Ok(mut permission) = app.devices.changes(&start.device_id).await else {
        return;
    };
    if app
        .devices
        .permitted(&start.device_id, "audio.input")
        .await
        .is_err()
        || app
            .devices
            .permitted(&start.device_id, "audio.output")
            .await
            .is_err()
    {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    let (_lease, mut cancelled) = match app.audio.reserve(&start).await {
        Ok(v) => v,
        Err(code) => {
            fail(&mut socket, &start.session_id, code).await;
            return;
        }
    };
    let Some(engine) = &app.audio.engine else {
        fail(&mut socket, &start.session_id, "unsupported_capability").await;
        return;
    };
    if !send(
        &mut socket,
        w::AudioFrame {
            frame: Some(w::audio_frame::Frame::Start(start.clone())),
            ..Default::default()
        },
    )
    .await
    {
        return;
    }
    let (input, inputs) = mpsc::channel(8);
    let mut input = Some(input);
    let (output, mut outputs) = mpsc::channel(8);
    let engine_task =
        app.audio
            .tasks
            .spawn(engine.run((*start).clone(), inputs, output, cancelled.clone()));
    let device_session = matches!(principal, Principal::Device(_, _));
    let cancelled_session = auth::cancelled(principal);
    tokio::pin!(cancelled_session);
    let mut shutdown = app.application.shutdown();
    let mut pending_input = None;
    let mut input_deadline = tokio::time::Instant::now();
    let mut expected = 0u64;
    let mut output_sequence = 0u64;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
    let reason = loop {
        if *cancelled.borrow() || *shutdown.borrow() {
            break "cancelled";
        }
        let queued_input = input.clone();
        tokio::select! {biased;
            _=&mut cancelled_session=>break if device_session {"permission_required"} else {"unauthorized"},
            _=tokio::time::sleep_until(deadline)=>break "timeout",
            _=shutdown.changed()=>break "cancelled",
            _=cancelled.changed()=>break "cancelled",
            _=permission.changed()=>break "permission_required",
            ready = async move {
                match queued_input {
                    Some(sender) => tokio::time::timeout_at(input_deadline, sender.reserve_owned()).await,
                    None => std::future::pending().await,
                }
            }, if pending_input.is_some() => match ready {
                Ok(Ok(permit)) => { permit.send(pending_input.take().expect("pending input")); },
                _ => break "resource_limit",
            },
            incoming=socket.recv(), if pending_input.is_none()=>match incoming {
                Some(Ok(Frame::Binary(bytes)))=> {
                    let Ok(frame)=w::AudioFrame::decode_from_slice(&bytes) else {break "invalid_request";};
                    match frame.frame {
                        Some(w::audio_frame::Frame::Chunk(chunk)) if input.is_some() && chunk.session_id==start.session_id && chunk.sequence==expected && !chunk.data.is_empty() && chunk.data.len()<=16*1024 && chunk.data.len().is_multiple_of(2) => {
                            if *cancelled.borrow() {break "cancelled";}
                            let Some(next)=expected.checked_add(1) else {break "resource_limit";};expected=next;
                            pending_input = Some(*chunk);
                            input_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                        }
                        Some(w::audio_frame::Frame::End(end)) if end.session_id==start.session_id && input.is_some() => {input.take();deadline=tokio::time::Instant::now()+Duration::from_secs(30);},
                        _=>break "invalid_request",
                    }
                }
                Some(Ok(Frame::Ping(data)))=> {if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(Frame::Pong(data))).await,Ok(Ok(()))) {break "disconnected";}},
                Some(Ok(Frame::Pong(_)))=>{},
                _=>break "disconnected",
            },
            output=outputs.recv()=>match output {
                Some(frame)=> {
                    let valid=match &frame.frame {
                        Some(w::audio_frame::Frame::Chunk(chunk))=>chunk.session_id==start.session_id && chunk.sequence==output_sequence && !chunk.data.is_empty() && chunk.data.len()<=16*1024 && chunk.data.len().is_multiple_of(2),
                        Some(w::audio_frame::Frame::End(end))=>end.session_id==start.session_id,
                        Some(w::audio_frame::Frame::Failure(error))=>error.session_id==start.session_id,
                        _=>false,
                    };
                    if !valid {break "invalid_engine_output";}
                    if matches!(frame.frame,Some(w::audio_frame::Frame::Chunk(_))) {let Some(next)=output_sequence.checked_add(1) else {break "resource_limit";};output_sequence=next;}
                    if *cancelled.borrow() {break "cancelled";}
                    // Stop/revoke can interrupt a slow output send.
                    tokio::select! {biased;
                        _=cancelled.changed()=>break "cancelled",
                        _=permission.changed()=>break "permission_required",
                        sent=send(&mut socket,frame)=>if !sent {break "disconnected";}
                    }
                }
                None=>break "ended",
            }
        }
    };
    drop(input);
    engine_task.abort();
    let _ = engine_task.await;
    if reason == "ended" {
        let _ = send(
            &mut socket,
            w::AudioFrame {
                frame: Some(w::audio_frame::Frame::End(Box::new(w::AudioEnd {
                    session_id: start.session_id.clone(),
                    ..Default::default()
                }))),
                ..Default::default()
            },
        )
        .await;
    } else if reason != "disconnected" {
        fail(&mut socket, &start.session_id, reason).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
}

#[cfg(test)]
mod ticket_tests {
    use super::*;
    struct EngineFixture;
    impl Engine for EngineFixture {
        fn run(
            &self,
            _: w::AudioStart,
            _: mpsc::Receiver<w::AudioChunk>,
            _: mpsc::Sender<w::AudioFrame>,
            _: watch::Receiver<bool>,
        ) -> EngineFuture {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn only_unexpired_once_issued_audio_tickets_can_start() {
        let audio = Audio::with_engine(Arc::new(EngineFixture));
        let (permission, changed) = watch::channel(());
        let request = w::PrepareAudioRequest {
            companion_id: "companion".into(),
            device_id: "device".into(),
            codec: "pcm_s16le".into(),
            sample_rate: 24_000,
            channels: 1,
            ..Default::default()
        };
        let start = audio
            .prepare(request.clone(), changed.clone())
            .await
            .unwrap();
        let restarted = Audio::with_engine(Arc::new(EngineFixture));
        assert!(restarted.reserve(&start).await.is_err());
        let mut modified = start.clone();
        modified.sample_rate = 16_000;
        assert!(audio.reserve(&modified).await.is_err());
        let (lease, _) = audio.reserve(&start).await.unwrap();
        drop(lease);
        assert!(audio.reserve(&start).await.is_err());
        let stale = audio
            .prepare(request.clone(), changed.clone())
            .await
            .unwrap();
        permission.send_replace(());
        assert!(audio.reserve(&stale).await.is_err());
        let expired = audio
            .prepare(request, permission.subscribe())
            .await
            .unwrap();
        audio
            .state
            .lock()
            .await
            .prepared
            .get_mut(&expired.session_id)
            .unwrap()
            .expires = tokio::time::Instant::now();
        assert!(audio.reserve(&expired).await.is_err());
    }
}
