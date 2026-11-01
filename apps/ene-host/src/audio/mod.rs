//! A bounded protobuf WebSocket transport. Speech engines plug in here; the
//! transport never pretends that microphone data alone implements STT/TTS.
#[cfg(test)]
mod ticket_tests;
mod transport;

pub(crate) use transport::upgrade;

use ene_rpc::rpc as w;
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
