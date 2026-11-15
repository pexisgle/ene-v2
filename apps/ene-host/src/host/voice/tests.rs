use super::*;
use crate::host::db::clients::ClientDeclaration;
use crate::host::permission::{self, Approval, ApprovalDecision, ApprovalStatus, Approved};
use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
use rig_ecs::tools::ToolRequest;

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

struct FakeStt;

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

fn store_with_client() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    store
        .register_client(&ClientDeclaration {
            id: "c1",
            name: "desk",
            platform: "linux",
            capabilities: &[],
            specs: &[],
            expected_revision: 0,
        })
        .unwrap();
    store.activate("ene", Some("c1"), 0).unwrap();
    (root, store)
}

/// The world an approval-binding test needs: the store, the live call map and
/// the notification outbox.
fn voice_world(store: &Store) -> World {
    let mut world = test_world();
    world.insert_resource(store.clone());
    world.insert_resource(permission::Approvals::default());
    world.insert_resource(Notifier::new());
    world
}

fn client_binding(world: &World) -> InputBinding {
    bind_input(
        world,
        &w::AudioInputChunk {
            client_id: "c1".into(),
            activation_revision: 1,
            ..Default::default()
        },
    )
    .unwrap()
}

/// A pending device approval, as the gate issues it for `c1`, revision 1.
fn pending_approval(request_id: &str) -> Approval {
    let arguments = serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}});
    Approval {
        request_id: request_id.into(),
        session_id: "ene".into(),
        run_id: String::new(),
        work_id: None,
        action_payload_hash: permission::canonicalise("device_request", &arguments).1,
        tool_name: "device_request".into(),
        normalized_arguments: arguments.to_string(),
        status: ApprovalStatus::Pending,
        created_at: crate::host::db::now(),
        resolved_at: None,
        client_id: Some("c1".into()),
        activation_revision: Some(1),
        capability: Some("device.files".into()),
    }
}

/// Registers the live call that `submit` releases, as the gate would.
fn hold_approval(world: &mut World, approval: &Approval) -> Entity {
    let call = world
        .spawn(ToolRequest {
            call: ToolCall::new(
                CallId::from_wire("call-1"),
                ToolFunction::new(
                    ToolName::new("device_request").unwrap(),
                    serde_json::json!({
                        "capability": "device.files",
                        "input": {"path": "/tmp/a"},
                    }),
                ),
            ),
        })
        .id();
    let marker = world.spawn(()).id();
    world
        .resource_mut::<permission::Approvals>()
        .by_request
        .insert(
            approval.request_id.clone(),
            permission::Hold {
                call,
                hold: marker,
                approval: approval.clone(),
            },
        );
    call
}

fn chunks(events: &[w::AudioOutputEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            Some(w::audio_output_event::Event::Chunk(chunk)) => Some(chunk.sequence_no),
            _ => None,
        })
        .collect()
}

fn finished(events: &[w::AudioOutputEvent]) -> Option<u32> {
    events.iter().find_map(|event| match &event.event {
        Some(w::audio_output_event::Event::Finished(finished)) => Some(finished.total_sentences),
        _ => None,
    })
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
async fn skipped_sentences_still_finish_the_utterance() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1", 1);
    voice.begin("ene", Arc::new(FakeTts { fail: true }), String::new());
    voice.speak("ene", "first。second。");
    voice.end_turn("ene");
    for sequence in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event.event, Some(w::audio_output_event::Event::Skipped(skipped)) if skipped.sequence_no == sequence)
        );
    }
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished(&[event]), Some(2));
}

#[tokio::test]
async fn normalization_errors_skip_sentences_without_losing_order() {
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
    voice.publish("ene", "c1", 1);
    voice.begin("ene", Arc::new(InvalidTts), String::new());
    voice.speak("ene", "odd。zero。empty。valid。");
    voice.end_turn("ene");
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            let event = events.recv().await.unwrap();
            let done = finished(std::slice::from_ref(&event)) == Some(4);
            collected.push(event);
            if done {
                break collected;
            }
        }
    })
    .await
    .unwrap();
    let skipped: Vec<_> = collected
        .iter()
        .filter_map(|event| match &event.event {
            Some(w::audio_output_event::Event::Skipped(skipped)) => Some(skipped.sequence_no),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, [0, 1]);
    assert_eq!(chunks(&collected), [3]);
    assert_eq!(finished(&collected), Some(4));
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
    voice.publish("ene", "c1", 1);
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
    tokio::time::timeout(Duration::from_secs(5), async {
        let event = events.recv().await.unwrap();
        match event.event {
            Some(w::audio_output_event::Event::Chunk(chunk)) => {
                assert_eq!(chunk.generation_id, generation);
                assert_eq!(chunk.pcm_data, pcm_bytes("new。".bytes().map(i16::from)));
            }
            other => panic!("expected current audio, got {other:?}"),
        }
        let event = events.recv().await.unwrap();
        assert_eq!(finished(&[event]), Some(1));
    })
    .await
    .unwrap();
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_flush_invalidates_the_older_generation() {
    let mut world = test_world();
    let subscription = world.resource::<Voice>().register("c1");
    let mut events = subscription.control;
    let _audio = subscription.receiver;
    world.resource::<Voice>().publish("ene", "c1", 1);
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
fn a_stale_playback_report_is_ignored() {
    let (_root, store) = store_with_client();
    let mut world = test_world();
    world.insert_resource(store.clone());
    world.insert_resource(Notifier::new());
    world.resource::<Voice>().publish("ene", "c1", 1);
    on_stream_text(&mut world, "ene", "こんにちは。");
    on_turn_end(&mut world, "ene");
    let owner = current_owner(&world, "ene");
    let utterance = owner.utterance.clone().unwrap();
    assert_eq!(utterance.total_sentences, 1);
    assert!(utterance.deadline.is_some());

    let report = |generation_id| w::PlaybackStatusReport {
        utterance_id: utterance.id.clone(),
        generation_id,
        status: w::playback_status_report::Status::PLAYBACK_COMPLETED.into(),
        completed_sentence_count: 1,
        client_id: "c1".into(),
        activation_revision: 1,
        ..Default::default()
    };
    // A stale generation is refused and leaves the utterance owning the floor.
    assert!(
        !report_playback(&mut world, report(owner.generation_id + 1))
            .unwrap()
            .acknowledged
    );
    assert!(current_owner(&world, "ene").utterance.is_some());

    // The current generation is acknowledged and releases the floor; the host
    // keeps no separate report row, only the RPC-visible acknowledgement.
    let mut events = world.resource::<Notifier>().subscribe();
    assert!(
        report_playback(&mut world, report(owner.generation_id))
            .unwrap()
            .acknowledged
    );
    assert!(current_owner(&world, "ene").utterance.is_none());
    assert!(world.resource::<Voice>().floor_holder().is_none());
    world.resource_mut::<Notifier>().flush();
    assert!(matches!(
        events.try_recv().expect("speech_completed is broadcast").event,
        Some(w::notification::Event::Generic(event)) if event.kind == "speech_completed"
    ));
}

#[test]
fn a_voice_affirmative_answers_the_bound_approval() {
    let (_root, store) = store_with_client();
    let approval = pending_approval("a1");
    store.insert_approval(&approval).unwrap();
    let mut world = voice_world(&store);
    let call = hold_approval(&mut world, &approval);

    let binding = client_binding(&world);
    let speech = bind_speech_start(&world, &binding).expect("one pending approval is bound");
    assert_eq!(speech.speech_start_request_id, "a1");
    assert_eq!(speech.generation, 1);

    assert!(
        accept_transcription(
            &mut world,
            "t1",
            "はい、お願いします",
            &binding,
            Some(&speech)
        )
        .unwrap()
    );
    assert_eq!(
        store.approval("a1").unwrap().unwrap().status,
        ApprovalStatus::Approved
    );
    assert!(world.get::<Approved>(call).is_some());
    // The transcript is recorded normally, and the delegation claim is keyed
    // by its id.
    assert!(
        world
            .resource::<Voice>()
            .lock()
            .transcriptions
            .contains(&"t1".to_owned())
    );
    assert!(delegation::try_claim(&mut world, "t1", "ene"));
    assert!(!delegation::try_claim(&mut world, "t1", "kuro"));
}

#[test]
fn a_delayed_transcript_does_not_answer_a_later_approval() {
    let (_root, store) = store_with_client();
    let first = pending_approval("a1");
    store.insert_approval(&first).unwrap();
    let mut world = voice_world(&store);
    hold_approval(&mut world, &first);
    let binding = client_binding(&world);
    let speech = bind_speech_start(&world, &binding).unwrap();

    // The card resolved A while the STT ran, and B was issued afterwards.
    store
        .resolve_approval("a1", ApprovalStatus::Denied)
        .unwrap();
    let second = pending_approval("a2");
    store.insert_approval(&second).unwrap();
    hold_approval(&mut world, &second);

    // The delayed "はい" carries A's binding and must not answer B.
    assert!(accept_transcription(&mut world, "t1", "はい", &binding, Some(&speech)).unwrap());
    assert_eq!(
        store.approval("a1").unwrap().unwrap().status,
        ApprovalStatus::Denied
    );
    assert_eq!(
        store.approval("a2").unwrap().unwrap().status,
        ApprovalStatus::Pending
    );
}

#[test]
fn a_moved_generation_discards_the_bound_approval() {
    let (_root, store) = store_with_client();
    let approval = pending_approval("a1");
    store.insert_approval(&approval).unwrap();
    let mut world = voice_world(&store);
    hold_approval(&mut world, &approval);
    let binding = client_binding(&world);
    let speech = bind_speech_start(&world, &binding).unwrap();

    // The companion moved to a new generation while the STT ran.
    store.activate("ene", Some("c1"), 1).unwrap();
    assert!(!accept_transcription(&mut world, "t1", "はい", &binding, Some(&speech)).unwrap());
    assert_eq!(
        store.approval("a1").unwrap().unwrap().status,
        ApprovalStatus::Pending
    );
}

#[test]
fn two_pending_approvals_disarm_voice_approval() {
    let (_root, store) = store_with_client();
    store.insert_approval(&pending_approval("a1")).unwrap();
    store.insert_approval(&pending_approval("a2")).unwrap();
    let mut world = voice_world(&store);
    let binding = client_binding(&world);

    // Two pending requests are ambiguous: the screen card must choose.
    assert!(bind_speech_start(&world, &binding).is_none());
    assert!(accept_transcription(&mut world, "t1", "はい", &binding, None).unwrap());
    for request in ["a1", "a2"] {
        assert_eq!(
            store.approval(request).unwrap().unwrap().status,
            ApprovalStatus::Pending
        );
    }
}

#[test]
fn ambiguous_speech_never_grants_approval() {
    assert_eq!(
        approval_intent("いや、いいえ、いいです"),
        Some(ApprovalDecision::Deny)
    );
    assert_eq!(approval_intent("not now"), None);
    assert_eq!(approval_intent("そうですね"), None);
}

fn complete_playback(world: &mut World) -> bool {
    on_stream_text(world, "ene", "こんにちは。");
    on_turn_end(world, "ene");
    let utterance = current_owner(world, "ene").utterance.clone().unwrap();
    let report = w::PlaybackStatusReport {
        utterance_id: utterance.id,
        generation_id: utterance.generation_id,
        status: w::playback_status_report::Status::PLAYBACK_COMPLETED.into(),
        completed_sentence_count: utterance.total_sentences,
        client_id: "c1".into(),
        activation_revision: 1,
        ..Default::default()
    };
    let acknowledged = report_playback(world, report).unwrap().acknowledged;
    world.resource_mut::<Notifier>().flush();
    acknowledged
}

fn relayed(events: &mut tokio::sync::broadcast::Receiver<w::Notification>) -> usize {
    let mut count = 0;
    while let Ok(notification) = events.try_recv() {
        if matches!(
            notification.event,
            Some(w::notification::Event::Generic(event)) if event.kind == "speech_completed"
        ) {
            count += 1;
        }
    }
    count
}

#[test]
fn the_autonomous_relay_stops_after_five_completions() {
    let (_root, store) = store_with_client();
    let mut world = voice_world(&store);
    world.resource::<Voice>().publish("ene", "c1", 1);
    let mut events = world.resource::<Notifier>().subscribe();

    for _ in 0..MAX_RELAY + 1 {
        assert!(complete_playback(&mut world));
    }
    // Five peer prompts are relayed; the sixth completed utterance releases
    // the floor but no longer invites a peer to answer.
    assert_eq!(relayed(&mut events), MAX_RELAY as usize);

    // An explicit user turn opens the budget again.
    note_user_input(&world);
    for _ in 0..2 {
        assert!(complete_playback(&mut world));
    }
    assert_eq!(relayed(&mut events), 2);
}

#[test]
fn a_moved_activation_discards_the_transcription() {
    let (_root, store) = store_with_client();
    let mut world = test_world();
    world.insert_resource(store.clone());
    let chunk = w::AudioInputChunk {
        client_id: "c1".into(),
        activation_revision: 1,
        ..Default::default()
    };
    let binding = bind_input(&world, &chunk).unwrap();
    assert!(accept_transcription(&mut world, "t1", "こんにちは", &binding, None).unwrap());
    assert!(
        world
            .resource::<Voice>()
            .lock()
            .transcriptions
            .contains(&"t1".to_owned())
    );

    // The HTTP call happened before the move; acceptance must revalidate.
    store.activate("ene", Some("c1"), 1).unwrap();
    assert!(!accept_transcription(&mut world, "t2", "さようなら", &binding, None).unwrap());
    assert!(
        !world
            .resource::<Voice>()
            .lock()
            .transcriptions
            .contains(&"t2".to_owned())
    );

    let moved = bind_input(
        &world,
        &w::AudioInputChunk {
            client_id: "c1".into(),
            activation_revision: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(accept_transcription(&mut world, "t3", "ただいま", &moved, None).unwrap());
    assert!(
        world
            .resource::<Voice>()
            .lock()
            .transcriptions
            .contains(&"t3".to_owned())
    );
}

fn pcm_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
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

#[test]
fn wrapped_wav_preserves_mono_i16_samples() {
    for samples in [vec![], vec![i16::MIN, -1, 0, 1, i16::MAX]] {
        for rate in STT_SAMPLE_RATES {
            let pcm = pcm_bytes(samples.iter().copied());
            let wav = wav_wrap(&pcm, rate).unwrap();
            let mut reader = hound::WavReader::new(Cursor::new(&wav)).unwrap();
            assert_eq!(reader.spec().channels, 1);
            assert_eq!(reader.spec().bits_per_sample, 16);
            assert_eq!(reader.spec().sample_format, hound::SampleFormat::Int);
            assert_eq!(reader.spec().sample_rate, rate);
            assert_eq!(reader.duration() as usize, samples.len());
            assert_eq!(
                reader
                    .samples::<i16>()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap(),
                samples
            );
            assert_eq!(wav.len(), 44 + pcm.len());
            assert_eq!(&wav[44..], pcm);
            assert_eq!(
                u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
                36 + pcm.len()
            );
            assert_eq!(
                u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize,
                pcm.len()
            );
            let mut reference = Cursor::new(Vec::new());
            let mut writer = hound::WavWriter::new(&mut reference, reader.spec()).unwrap();
            for sample in &samples {
                writer.write_sample(*sample).unwrap();
            }
            writer.finalize().unwrap();
            assert_eq!(wav, reference.into_inner());
        }
    }
    assert!(wav_wrap(&[0], SAMPLE_RATE).is_err());
    assert!(wav_wrap(&[], 0).is_err());
    assert!(wav_wrap(&[], u32::MAX / 2 + 1).is_err());
    assert!(wav_wrap(&[], u32::MAX / 2).is_ok());
}

#[test]
fn wav_counts_reject_riff_and_integer_overflow_without_allocating() {
    assert_eq!(wav_sample_count(0).unwrap(), 0);
    let max = ((u32::MAX - 36) / 2) as usize;
    #[cfg(target_pointer_width = "64")]
    assert_eq!(wav_sample_count(max).unwrap(), max as u32);
    assert!(wav_sample_count(max + 1).is_err());
    assert!(wav_sample_count(u32::MAX as usize).is_err());
    assert!(wav_sample_count(usize::MAX).is_err());
}

#[derive(Clone, Copy, Debug)]
enum WavFailure {
    Create,
    Samples,
    FinalizeSeek,
    FinalizeWrite,
    FinalizeFlush,
}

struct FailingWav {
    bytes: Cursor<Vec<u8>>,
    failure: WavFailure,
}

impl std::io::Write for FailingWav {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let fails = match self.failure {
            WavFailure::Create => self.bytes.position() == 0,
            WavFailure::Samples => self.bytes.position() >= 44,
            WavFailure::FinalizeWrite => self.bytes.position() == 4,
            _ => false,
        };
        if fails {
            return Err(std::io::Error::other("injected WAV write failure"));
        }
        std::io::Write::write(&mut self.bytes, bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if matches!(self.failure, WavFailure::FinalizeFlush) {
            return Err(std::io::Error::other("injected WAV flush failure"));
        }
        Ok(())
    }
}

impl std::io::Seek for FailingWav {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        if matches!(self.failure, WavFailure::FinalizeSeek) {
            return Err(std::io::Error::other("injected WAV seek failure"));
        }
        std::io::Seek::seek(&mut self.bytes, position)
    }
}

#[test]
fn wav_creation_batch_flush_and_finalization_errors_propagate() {
    for (failure, stage) in [
        (WavFailure::Create, "header could not be created"),
        (WavFailure::Samples, "samples could not be written"),
        (WavFailure::FinalizeSeek, "could not be finalized"),
        (WavFailure::FinalizeWrite, "could not be finalized"),
        (WavFailure::FinalizeFlush, "could not be finalized"),
    ] {
        let mut sink = FailingWav {
            bytes: Cursor::new(Vec::new()),
            failure,
        };
        let error =
            write_wav(&mut sink, &pcm_bytes([i16::MIN, 0, i16::MAX]), SAMPLE_RATE).unwrap_err();
        assert!(error.contains(stage), "{failure:?}: {error}");
        assert!(error.contains("injected WAV"), "{failure:?}: {error}");
    }
}

/// A stereo WAV in the 16-bit shape the TTS endpoints return.
fn stereo_wav(frames: &[(i16, i16)]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(frames.len() * 4);
    for (left, right) in frames {
        pcm.extend_from_slice(&left.to_le_bytes());
        pcm.extend_from_slice(&right.to_le_bytes());
    }
    let mut wav = wav_wrap(&pcm, SAMPLE_RATE).unwrap();
    wav[22..24].copy_from_slice(&2u16.to_le_bytes());
    wav[28..32].copy_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    wav[32..34].copy_from_slice(&4u16.to_le_bytes());
    wav
}

#[test]
fn a_negative_second_stereo_channel_is_averaged_not_clamped_positive() {
    let wav = stereo_wav(&[(0, -1_000), (-32_768, -32_768), (3_000, -1_000)]);
    let (pcm, rate) = parse_wav(&wav).unwrap();
    assert_eq!(rate, SAMPLE_RATE);
    let samples: Vec<i16> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    assert_eq!(samples, [-500, -32_768, 1_000]);
}

#[tokio::test]
async fn stt_requests_send_decodable_wav_samples() {
    use crate::host::settings::{CredentialState, SttConnection};
    use axum::{Router, routing::post};

    for raw in [false, true] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/audio/transcriptions",
            post(move |request: axum::extract::Request| async move {
                let content_type = request.headers()[reqwest::header::CONTENT_TYPE]
                    .to_str()
                    .unwrap();
                if raw {
                    assert_eq!(content_type, "application/octet-stream");
                } else {
                    assert!(content_type.starts_with("multipart/form-data;"));
                }
                let body = axum::body::to_bytes(request.into_body(), 4096)
                    .await
                    .unwrap();
                let start = body.windows(4).position(|bytes| bytes == b"RIFF").unwrap();
                if raw {
                    assert_eq!(start, 0);
                }
                let mut reader = hound::WavReader::new(Cursor::new(&body[start..])).unwrap();
                assert_eq!(reader.spec().sample_rate, SAMPLE_RATE);
                assert_eq!(reader.spec().channels, 1);
                assert_eq!(reader.spec().bits_per_sample, 16);
                assert_eq!(
                    reader
                        .samples::<i16>()
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .unwrap(),
                    [i16::MIN, 0, i16::MAX]
                );
                if raw { "heard" } else { r#"{"text":"heard"}"# }
            }),
        );
        let _server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let engine = HttpEngine::new(
            Connection {
                id: "wav-stt".into(),
                name: "WAV STT".into(),
                enabled: true,
                kind: ConnectionKind::Stt,
                config: ConnectionConfig::Stt(SttConnection {
                    endpoint: Endpoint {
                        adapter: "openai".into(),
                        endpoint: format!("http://{address}"),
                        options:
                            serde_json::json!({"protocol": if raw { "raw" } else { "openai" }})
                                .to_string(),
                        ..Default::default()
                    },
                    language_hints: Vec::new(),
                }),
                credential_state: CredentialState::Absent,
                revision: 1,
                last_check: None,
            },
            None,
            ConnectionKind::Stt,
        )
        .unwrap();
        assert_eq!(
            engine
                .transcribe(pcm_bytes([i16::MIN, 0, i16::MAX]), SAMPLE_RATE)
                .await
                .unwrap(),
            "heard"
        );
        assert!(engine.transcribe(vec![0], SAMPLE_RATE).await.is_err());
    }
}

#[tokio::test]
async fn configured_speech_requests_reject_oversized_audio() {
    use crate::host::settings::{CredentialState, TtsConnection};
    use axum::{Router, body::Body, response::Response, routing::post};

    for chunked in [false, true] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/custom",
            post(move |request: axum::extract::Request| async move {
                let body = axum::body::to_bytes(request.into_body(), 2048)
                    .await
                    .unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["model"], "test-model");
                assert_eq!(request["voice"], "test-voice");
                assert_eq!(request["speed"], 1.25);
                assert_eq!(request["input"], "hello");
                assert_eq!(request["response_format"], "pcm");
                let body = if chunked {
                    Body::from_stream(futures_util::stream::iter(
                        (0..=MAX_BODY_BYTES / AUDIO_CHUNK_BYTES).map(|_| {
                            Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                                &[0; AUDIO_CHUNK_BYTES],
                            ))
                        }),
                    ))
                } else {
                    Body::from(vec![0; MAX_BODY_BYTES + 1])
                };
                Response::new(body)
            }),
        );
        let _server = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let engine = HttpEngine::new(
            Connection {
                id: "speech-limit".into(),
                name: "speech limit".into(),
                enabled: true,
                kind: ConnectionKind::Tts,
                config: ConnectionConfig::Tts(TtsConnection {
                    endpoint: Endpoint {
                        adapter: "openai".into(),
                        endpoint: format!("http://{address}"),
                        options_schema: String::new(),
                        options: serde_json::json!({
                            "path": "/custom", "model": "test-model",
                            "voice": "test-voice", "speed": 1.25
                        })
                        .to_string(),
                    },
                    voice_ids: Vec::new(),
                }),
                credential_state: CredentialState::Absent,
                revision: 1,
                last_check: None,
            },
            None,
            ConnectionKind::Tts,
        )
        .unwrap();
        let error = engine
            .synthesize("hello".into(), String::new())
            .await
            .unwrap_err();
        assert!(error.contains("more data"), "chunked={chunked}: {error}");
    }
}

#[tokio::test]
async fn the_worker_orders_audio_and_finishes_the_utterance() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1", 1);
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
        while let Some(event) = events.recv().await {
            let done = finished(std::slice::from_ref(&event)) == Some(2);
            collected.push(event);
            if done {
                break;
            }
        }
        collected
    })
    .await
    .expect("the worker finishes the utterance");
    assert_eq!(chunks(&collected), [0, 1]);
    assert_eq!(finished(&collected), Some(2));
    for event in &collected {
        if let Some(w::audio_output_event::Event::Chunk(chunk)) = &event.event {
            let first = chunk.sequence_no == 0;
            let pcm = pcm_bytes((0..128).map(|index| if first { index * 97 } else { -index * 53 }));
            let rate = if first { 48_000 } else { 44_100 };
            assert_eq!(chunk.sample_rate, SAMPLE_RATE);
            assert_eq!(chunk.pcm_data, wire_pcm(pcm, rate, &mut None).unwrap());
        }
    }
}

#[tokio::test]
async fn a_stalled_owner_does_not_block_another() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let slow = voice.register("c1");
    let mut slow_control = slow.control;
    let _slow_audio = slow.receiver; // never drained: the owner is stalled
    let mut fast = voice.register("c2").receiver;
    voice.publish("ene", "c1", 1);
    voice.publish("kuro", "c2", 1);
    voice.begin("ene", Arc::new(BigTts), String::new());
    voice.speak("ene", "slow。");
    voice.end_turn("ene");
    voice.begin("kuro", Arc::new(BigTts), String::new());
    voice.speak("kuro", "fast。");
    voice.lock().floor = Some("kuro".into());
    voice.end_turn("kuro");

    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        while let Some(event) = fast.recv().await {
            let done = finished(std::slice::from_ref(&event)) == Some(1);
            collected.push(event);
            if done {
                break;
            }
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
        flush.event,
        Some(w::audio_output_event::Event::Flush(flush))
            if flush.reason == w::FlushReason::Cancelled
    ));
    assert!(!voice.lock().owners.contains_key("ene"));
}
