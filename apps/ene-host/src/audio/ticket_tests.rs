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
