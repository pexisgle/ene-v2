use super::*;
use crate::error::VoiceHttpError;
use crate::pcm::SAMPLE_RATE;
use std::time::Duration;

fn pcm_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
    samples.into_iter().flat_map(i16::to_le_bytes).collect()
}

struct FakeTts {
    fail: bool,
}

impl TtsEngine for FakeTts {
    fn synthesize<'a>(
        &'a self,
        text: String,
        _: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            if self.fail {
                return Err(VoiceHttpError::Engine("mock tts failed".into()));
            }
            Ok((
                text.bytes()
                    .flat_map(|byte| i16::from(byte).to_le_bytes())
                    .collect(),
                SAMPLE_RATE,
            ))
        })
    }
}

struct BigTts;

impl TtsEngine for BigTts {
    fn synthesize<'a>(
        &'a self,
        text: String,
        _: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            let bytes = if text.starts_with("slow") {
                5 * 1024 * 1024
            } else {
                64
            };
            Ok((vec![0u8; bytes], SAMPLE_RATE))
        })
    }
}

fn chunks(events: &[VoiceOutputEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            VoiceOutputEvent::Chunk(chunk) => Some(chunk.sequence_no),
            _ => None,
        })
        .collect()
}

fn finished(events: &[VoiceOutputEvent]) -> Option<u32> {
    events.iter().find_map(|event| match event {
        VoiceOutputEvent::Finished(finished) => Some(finished.total_sentences),
        _ => None,
    })
}

async fn settle(voice: &VoicePipeline) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while voice.has_active_work() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the synthesis work drains");
}

#[test]
fn sentences_split_on_the_design_terminators() {
    let mut buffer = String::from("おはよう。今日はいい天気ですね！明日は？\n");
    assert_eq!(
        split_sentences(&mut buffer),
        vec!["おはよう。", "今日はいい天気ですね！", "明日は？"]
    );
    assert!(buffer.is_empty());

    let mut buffer = String::from("まだ終わっていない");
    assert!(split_sentences(&mut buffer).is_empty());
    assert_eq!(
        flush_sentences(&mut buffer).as_deref(),
        Some("まだ終わっていない")
    );

    let mut buffer = "あ".repeat(MAX_SENTENCE_CHARS + 5);
    let sentences = split_sentences(&mut buffer);
    assert_eq!(sentences.len(), 1);
    assert_eq!(sentences[0].chars().count(), MAX_SENTENCE_CHARS);
    assert_eq!(buffer.chars().count(), 5);
}

#[tokio::test]
async fn failed_sentences_add_no_audio_and_the_utterance_still_finishes() {
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    voice.begin("ene", Arc::new(FakeTts { fail: true }), String::new());
    voice.speak("ene", "first。second。");
    voice.end_turn("ene");
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished(std::slice::from_ref(&event)), Some(2));
    assert!(voice.floor_holder().is_none());
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn the_finished_event_is_the_last_audio_event_of_the_utterance() {
    struct HeldTts {
        started: Notify,
        release: Notify,
    }
    impl TtsEngine for HeldTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                if text.starts_with("held") {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok((pcm_bytes([100; 32]), SAMPLE_RATE))
            })
        }
    }
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    let engine = Arc::new(HeldTts {
        started: Notify::new(),
        release: Notify::new(),
    });
    voice.begin("ene", engine.clone(), String::new());
    voice.speak("ene", "held。free。");
    tokio::time::timeout(Duration::from_secs(5), engine.started.notified())
        .await
        .unwrap();
    voice.end_turn("ene");
    assert!(voice.floor_holder().is_none());
    assert!(events.try_recv().is_err());
    engine.release.notify_one();
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            collected.push(events.recv().await.unwrap());
            if finished(&collected).is_some() {
                break collected;
            }
        }
    })
    .await
    .expect("the utterance still announces its end");
    assert_eq!(chunks(&collected), [0, 1]);
    assert_eq!(finished(&collected), Some(2));
    let announces = collected
        .iter()
        .filter(|event| matches!(event, VoiceOutputEvent::Finished(_)))
        .count();
    assert_eq!(announces, 1);
    assert!(matches!(
        collected.last(),
        Some(VoiceOutputEvent::Finished(_))
    ));
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_failed_sentence_finishes_the_utterance_after_its_audio() {
    struct PartialTts;
    impl TtsEngine for PartialTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                if text.starts_with("bad") {
                    return Err(VoiceHttpError::Engine("mock tts failed".into()));
                }
                Ok((pcm_bytes([100; 32]), SAMPLE_RATE))
            })
        }
    }
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    voice.begin("ene", Arc::new(PartialTts), String::new());
    voice.speak("ene", "bad。good。");
    voice.end_turn("ene");
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            collected.push(events.recv().await.unwrap());
            if finished(&collected).is_some() {
                break collected;
            }
        }
    })
    .await
    .expect("the utterance finishes without the failed sentence");
    assert_eq!(chunks(&collected), [1]);
    assert_eq!(finished(&collected), Some(2));
    assert!(matches!(
        collected.last(),
        Some(VoiceOutputEvent::Finished(_))
    ));
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn normalization_failures_add_no_audio_and_the_utterance_still_finishes() {
    struct InvalidTts;
    impl TtsEngine for InvalidTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                Ok(if text.starts_with("odd") {
                    (vec![0], 48_000)
                } else if text.starts_with("zero") {
                    (vec![0, 0], 0)
                } else if text.starts_with("empty") {
                    (Vec::new(), 48_000)
                } else {
                    (pcm_bytes([100; 16]), 48_000)
                })
            })
        }
    }
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    voice.begin("ene", Arc::new(InvalidTts), String::new());
    voice.speak("ene", "odd。zero。empty。valid。");
    voice.end_turn("ene");
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            collected.push(events.recv().await.unwrap());
            if finished(&collected) == Some(4) && chunks(&collected) == [3] {
                break collected;
            }
        }
    })
    .await
    .expect("only the valid sentence is spoken");
    assert_eq!(chunks(&collected), [3]);
    assert_eq!(finished(&collected), Some(4));
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_flushed_inflight_synthesis_cannot_emit_into_the_next_generation() {
    struct BlockedTts {
        started: Notify,
        release: Notify,
    }
    impl TtsEngine for BlockedTts {
        fn synthesize<'a>(
            &'a self,
            _: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                self.started.notify_one();
                self.release.notified().await;
                Ok((pcm_bytes([i16::MAX; 32]), 48_000))
            })
        }
    }
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    let engine = Arc::new(BlockedTts {
        started: Notify::new(),
        release: Notify::new(),
    });
    voice.begin("ene", engine.clone(), String::new());
    voice.speak("ene", "old。");
    tokio::time::timeout(Duration::from_secs(5), engine.started.notified())
        .await
        .unwrap();
    voice.flush("ene", FlushReason::BargeIn).unwrap();
    voice.begin("ene", Arc::new(FakeTts { fail: false }), String::new());
    voice.speak("ene", "new。");
    voice.end_turn("ene");
    let generation = voice.generation_of("ene").unwrap();
    engine.release.notify_one();
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            collected.push(events.recv().await.unwrap());
            if finished(&collected) == Some(1) && !chunks(&collected).is_empty() {
                break collected;
            }
        }
    })
    .await
    .unwrap();
    let audio: Vec<_> = collected
        .iter()
        .filter_map(|event| match event {
            VoiceOutputEvent::Chunk(chunk) => Some(chunk),
            _ => None,
        })
        .collect();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].generation_id, generation);
    assert_eq!(audio[0].pcm_data, pcm_bytes("new。".bytes().map(i16::from)));
    assert_eq!(finished(&collected), Some(1));
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn the_worker_orders_audio_and_finishes_the_utterance() {
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    struct DelayedTts(Notify);
    impl TtsEngine for DelayedTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                if text.starts_with("first") {
                    self.0.notified().await;
                } else {
                    self.0.notify_one();
                }
                let first = text.starts_with("first");
                let rate = if first { 48_000 } else { 44_100 };
                Ok((
                    pcm_bytes((0..128).map(|index| if first { index * 97 } else { -index * 53 })),
                    rate,
                ))
            })
        }
    }
    voice.begin("ene", Arc::new(DelayedTts(Notify::new())), String::new());
    voice.speak("ene", "first。second。");
    voice.end_turn("ene");
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        while collected.len() < 3 {
            collected.push(events.recv().await.unwrap());
        }
        collected
    })
    .await
    .expect("the worker finishes the utterance");
    assert_eq!(chunks(&collected), [0, 1]);
    assert_eq!(finished(&collected), Some(2));
    for event in &collected {
        if let VoiceOutputEvent::Chunk(chunk) = event {
            let first = chunk.sequence_no == 0;
            let pcm = pcm_bytes((0..128).map(|index| if first { index * 97 } else { -index * 53 }));
            let rate = if first { 48_000 } else { 44_100 };
            assert_eq!(chunk.sample_rate, SAMPLE_RATE);
            assert_eq!(chunk.pcm_data, wire_pcm(pcm, rate, &mut None).unwrap());
        }
    }
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_stalled_owner_does_not_block_another() {
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let slow = voice.register("c1");
    let mut slow_control = slow.control;
    let _slow_audio = slow.receiver;
    let mut fast = voice.register("c2").receiver;
    voice.publish("ene", "c1");
    voice.publish("kuro", "c2");
    voice.begin("ene", Arc::new(BigTts), String::new());
    voice.speak("ene", "slow。");
    voice.end_turn("ene");
    voice.begin("kuro", Arc::new(BigTts), String::new());
    voice.speak("kuro", "fast。");
    voice.end_turn("kuro");

    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        while collected.len() < 2 {
            collected.push(fast.recv().await.unwrap());
        }
        collected
    })
    .await
    .expect("a stalled owner does not stop another");
    assert_eq!(chunks(&collected), [0]);
    assert_eq!(finished(&collected), Some(1));

    let flush = tokio::time::timeout(Duration::from_secs(5), slow_control.recv())
        .await
        .expect("the stalled owner is flushed")
        .expect("a flush event");
    assert!(matches!(
        flush,
        VoiceOutputEvent::Flush(flush) if flush.reason == FlushReason::Cancelled
    ));
    assert!(!voice.has_owner("ene"));
}

#[tokio::test]
async fn f01_audio_events_carry_owning_companion_id() {
    let voice = VoicePipeline::new(tokio::runtime::Handle::current());
    let mut sub = voice.register("desk-1");
    voice.publish("ene", "desk-1");
    voice.publish("miku", "desk-1");

    assert!(voice.begin("miku", Arc::new(FakeTts { fail: false }), "1".into()));
    voice.speak("miku", "こんにちは。");
    let ended = voice.end_turn("miku");
    assert!(ended.is_some());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut chunk_companion = None;
    let mut finished_companion = None;
    while tokio::time::Instant::now() < deadline
        && (chunk_companion.is_none() || finished_companion.is_none())
    {
        if let Ok(event) = sub.receiver.try_recv() {
            match event {
                VoiceOutputEvent::Chunk(chunk) => {
                    chunk_companion = Some(chunk.companion_id);
                }
                VoiceOutputEvent::Finished(fin) => {
                    finished_companion = Some(fin.companion_id);
                }
                _ => {}
            }
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(chunk_companion.as_deref(), Some("miku"));
    assert_eq!(finished_companion.as_deref(), Some("miku"));
}
