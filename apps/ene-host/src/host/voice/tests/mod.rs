mod session_tests;

use super::*;
use crate::host::db::clients::ClientDeclaration;

pub(super) struct FakeTts {
    pub(super) fail: bool,
}

impl TtsEngine for FakeTts {
    fn synthesize<'a>(
        &'a self,
        text: String,
        _: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            if self.fail {
                return Err("mock tts failed".into());
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

/// Fills the owning client's queue so the delivery path must not await it.
pub(super) struct BigTts;

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

pub(super) struct FakeStt;

impl SttEngine for FakeStt {
    fn transcribe<'a>(&'a self, pcm: Vec<u8>, _: u32) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move { Ok(format!("{} bytes", pcm.len())) })
    }
}

/// A synchronous test has no runtime of its own; the leaked current-thread
/// runtime gives the voice tasks a home they never have to reach.
fn test_runtime() -> tokio::runtime::Handle {
    tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
        Box::leak(Box::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        ))
        .handle()
        .clone()
    })
}

fn current_owner(world: &World, companion: &str) -> Owner {
    world
        .resource::<Voice>()
        .lock()
        .owners
        .get(companion)
        .unwrap()
        .clone()
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(Voice::new(test_runtime()));
    world.insert_resource(Engines {
        tts: Some(Arc::new(FakeTts { fail: false })),
        stt: Some(Arc::new(FakeStt)),
    });
    world
}

pub(super) fn store_with_client() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    store
        .client_repo()
        .register_client(&ClientDeclaration {
            id: "c1",
            name: "desk",
            platform: "linux",
            capabilities: &[],
            specs: &[],
            expected_revision: 0,
        })
        .unwrap();
    store.client_repo().activate("ene", Some("c1")).unwrap();
    (root, store)
}

/// The world a voice test needs: the store and the notification outbox.
pub(super) fn voice_world(store: &Store) -> World {
    let mut world = test_world();
    world.insert_resource(store.clone());
    world.insert_resource(Notifier::new());
    world
}

pub(super) fn chunks(events: &[w::AudioOutputEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            Some(w::audio_output_event::Event::Chunk(chunk)) => Some(chunk.sequence_no),
            _ => None,
        })
        .collect()
}

pub(super) fn finished(events: &[w::AudioOutputEvent]) -> Option<u32> {
    events.iter().find_map(|event| match &event.event {
        Some(w::audio_output_event::Event::Finished(finished)) => Some(finished.total_sentences),
        _ => None,
    })
}

/// Waits until the synthesis tasks have drained, so the audio queue is final.
pub(super) async fn settle(voice: &Voice) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !voice.lock().works.is_empty() {
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

    // A long unpunctuated run is still spoken, in sentence-sized pieces.
    let mut buffer = "あ".repeat(MAX_SENTENCE_CHARS + 5);
    let sentences = split_sentences(&mut buffer);
    assert_eq!(sentences.len(), 1);
    assert_eq!(sentences[0].chars().count(), MAX_SENTENCE_CHARS);
    assert_eq!(buffer.chars().count(), 5);
}

#[tokio::test]
async fn failed_sentences_add_no_audio_and_the_utterance_still_finishes() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    voice.begin("ene", Arc::new(FakeTts { fail: true }), String::new());
    voice.speak("ene", "first。second。");
    voice.end_turn("ene");
    // The turn end frees the floor at once; the finished event waits only for
    // the two failed sentences to resolve, and no client report gates either.
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished(std::slice::from_ref(&event)), Some(2));
    assert!(voice.floor_holder().is_none());
    // Neither failed sentence contributes audio, and the utterance is forgotten
    // once its tasks drain.
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

/// The finished event is the last audio event of the utterance: a client may
/// read it as "the host is done", so no chunk may follow it.
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
    let voice = Voice::new(tokio::runtime::Handle::current());
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
    // The turn is over and the floor is free, but the utterance still owes
    // audio, so the client has nothing to see yet.
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
    // Both chunks precede the single finished event, which closes the utterance.
    assert_eq!(chunks(&collected), [0, 1]);
    assert_eq!(finished(&collected), Some(2));
    let announces = collected
        .iter()
        .filter(|event| {
            matches!(
                &event.event,
                Some(w::audio_output_event::Event::Finished(_))
            )
        })
        .count();
    assert_eq!(announces, 1);
    assert!(
        matches!(
            collected.last().map(|event| &event.event),
            Some(Some(w::audio_output_event::Event::Finished(_)))
        ),
        "the utterance finishes last"
    );
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

/// A sentence that fails contributes no audio and still releases its hold, so
/// the utterance finishes after the audio of the sentences that did synthesize.
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
                    return Err("mock tts failed".into());
                }
                Ok((pcm_bytes([100; 32]), SAMPLE_RATE))
            })
        }
    }
    let voice = Voice::new(tokio::runtime::Handle::current());
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
    assert!(
        matches!(
            collected.last().map(|event| &event.event),
            Some(Some(w::audio_output_event::Event::Finished(_)))
        ),
        "the utterance finishes last"
    );
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
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    voice.begin("ene", Arc::new(InvalidTts), String::new());
    voice.speak("ene", "odd。zero。empty。valid。");
    voice.end_turn("ene");
    // Only the last sentence reaches the wire; the three that fail to
    // normalize add nothing, and the finished event still counts all four.
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
    let voice = Voice::new(tokio::runtime::Handle::current());
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
    voice.flush("ene", w::FlushReason::BargeIn).unwrap();
    voice.begin("ene", Arc::new(FakeTts { fail: false }), String::new());
    voice.speak("ene", "new。");
    voice.end_turn("ene");
    let generation = voice.lock().owners["ene"].generation_id;
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
    // The flushed generation never reaches the wire; the new one plays its
    // sentence and finishes.
    let audio: Vec<_> = collected
        .iter()
        .filter_map(|event| match &event.event {
            Some(w::audio_output_event::Event::Chunk(chunk)) => Some(chunk),
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
async fn a_flush_invalidates_the_older_generation() {
    let mut world = test_world();
    let subscription = world.resource::<Voice>().register("c1");
    let mut events = subscription.control;
    let _audio = subscription.receiver;
    world.resource::<Voice>().publish("ene", "c1");
    on_stream_text(&mut world, "ene", "ひとつめ。");
    let first = current_owner(&world, "ene").generation_id;
    assert_eq!(first, 1);

    flush(&mut world, "ene", w::FlushReason::BargeIn).unwrap();
    match events.try_recv().expect("a flush reaches the client").event {
        Some(w::audio_output_event::Event::Flush(flush)) => {
            assert_eq!(flush.flush_generation_id, first);
            assert_eq!(flush.reason, w::FlushReason::BargeIn);
        }
        other => panic!("expected a flush, got {other:?}"),
    }
    // The delivery gate drops audio of the invalidated generation.
    let stale = audio_event(w::AudioChunkEnvelope {
        utterance_id: "one".into(),
        generation_id: first,
        ..Default::default()
    });
    assert!(matches!(
        deliver(&mut world.resource::<Voice>().lock(), "ene", first, stale),
        Dispatch::Stale
    ));

    on_stream_text(&mut world, "ene", "ふたつめ。");
    let second = current_owner(&world, "ene").generation_id;
    assert!(second > first);
    let current = audio_event(w::AudioChunkEnvelope {
        utterance_id: "two".into(),
        generation_id: second,
        ..Default::default()
    });
    assert!(matches!(
        deliver(
            &mut world.resource::<Voice>().lock(),
            "ene",
            second,
            current
        ),
        Dispatch::Sent
    ));
}

#[test]
fn turn_end_releases_the_floor_and_relays_the_utterance() {
    let (_root, store) = store_with_client();
    let mut world = voice_world(&store);
    world.resource::<Voice>().publish("ene", "c1");
    let mut events = world.resource::<Notifier>().subscribe();
    on_stream_text(&mut world, "ene", "こんにちは。");
    on_turn_end(&mut world, "ene");
    // The host owns the floor release: it happens when the turn ends, with no
    // client playback report in the loop.
    assert!(world.resource::<Voice>().floor_holder().is_none());
    world.resource_mut::<Notifier>().flush();
    let notification = events.try_recv().expect("speech_completed is broadcast");
    let Some(w::notification::Event::Generic(event)) = notification.event else {
        panic!("expected a generic host event");
    };
    assert_eq!(event.kind, "speech_completed");
    let payload = event
        .payload
        .as_option()
        .expect("the event names the utterance");
    let payload: serde_json::Value = serde_json::from_str(&payload.json).unwrap();
    assert_eq!(payload["companion_id"], "ene");
    assert_eq!(payload["client_id"], "c1");
    assert_eq!(payload["total_sentences"], 1);
    assert!(
        payload["utterance_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(
        payload["generation_id"]
            .as_u64()
            .is_some_and(|generation| generation > 0)
    );
}

#[test]
fn input_binding_validates_first_metadata_and_allows_omitted_followups() {
    let (_root, store) = store_with_client();
    store
        .client_repo()
        .register_client(&ClientDeclaration {
            id: "c2",
            name: "handheld",
            platform: "android",
            capabilities: &[],
            specs: &[],
            expected_revision: 0,
        })
        .unwrap();
    let world = voice_world(&store);
    assert!(matches!(bind_input(&world, ""), Err(HostError::Invalid(_))));
    assert!(matches!(
        bind_input(&world, "missing"),
        Err(HostError::NotFound(_))
    ));
    // A registered client that is not a companion's active client has nothing
    // the head may answer.
    assert!(matches!(
        bind_input(&world, "c2"),
        Err(HostError::Conflict(_))
    ));
    let binding = bind_input(&world, "c1").unwrap();
    assert!(binding.accepts("c1"));
    // A follow-up frame of the same stream may omit the metadata.
    assert!(binding.accepts(""));
    assert!(!binding.accepts("c2"));
}

pub(super) fn pcm_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
    samples.into_iter().flat_map(i16::to_le_bytes).collect()
}

#[test]
fn finite_clips_keep_the_rounded_sample_count() {
    for from in [16_000, 24_000, 32_000, 44_100, 48_000, 96_000] {
        for len in [0, 1, 2, 7, 127, 1_023, 1_024, 1_025, 4_097, 24_001] {
            let pcm = pcm_bytes((0..len).map(|index| (index * 997) as i16));
            let converted = resample_mono(&pcm, from, SAMPLE_RATE, &mut None).unwrap();
            let expected = (len as f64 * f64::from(SAMPLE_RATE) / f64::from(from)).round();
            assert_eq!(converted.len(), expected as usize * 2, "{from}, {len}");
            if from == SAMPLE_RATE {
                assert_eq!(converted, pcm);
            }
        }
    }
    assert!(
        resample_mono(&[0, 0], 96_000, SAMPLE_RATE, &mut None)
            .unwrap()
            .is_empty()
    );
    assert!(wire_pcm(vec![0], SAMPLE_RATE, &mut None).is_err());
    assert!(wire_pcm(Vec::new(), 0, &mut None).is_err());
}

#[test]
fn reused_resamplers_match_cold_bytes_without_leaking_between_clips() {
    let mut cache = None;
    for from in [44_100, 48_000, 96_000, 16_000, 44_100] {
        for len in [0, 1, 2, 17, 127, 1_023, 1_024, 4_097, 24_001] {
            let impulse = pcm_bytes((0..len).map(|index| if index == 0 { i16::MAX } else { 0 }));
            let signal = pcm_bytes((0..len).map(|index| (index * 997) as i16));
            let silence = vec![0; len * 2];
            for pcm in [&impulse, &silence, &signal, &silence, &signal] {
                let cold = resample_mono(pcm, from, SAMPLE_RATE, &mut None).unwrap();
                let reused = resample_mono(pcm, from, SAMPLE_RATE, &mut cache).unwrap();
                assert_eq!(reused, cold, "{from}, {len}");
                if pcm == &silence {
                    assert!(reused.iter().all(|byte| *byte == 0), "{from}, {len}");
                }
            }
            let identity = wire_pcm(signal.clone(), SAMPLE_RATE, &mut cache).unwrap();
            assert_eq!(identity, signal);
            assert!(wire_pcm(Vec::new(), from, &mut cache).unwrap().is_empty());
        }
    }
    let pcm = pcm_bytes([i16::MIN, i16::MAX, 0, -123, 123]);
    for (from, to) in [(48_000, 16_000), (48_000, 24_000), (48_000, 16_000)] {
        assert_eq!(
            resample_mono(&pcm, from, to, &mut cache).unwrap(),
            resample_mono(&pcm, from, to, &mut None).unwrap(),
        );
    }
}
