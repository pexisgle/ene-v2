use super::*;
use crate::host::db::clients::ClientDeclaration;
use crate::host::net;
use crate::host::permission::{self, Approval, ApprovalDecision, ApprovalStatus, Approved};
use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
use rig_ecs::tools::ToolRequest;

/// Deterministic PCM, optionally failing; the tests never touch the network.
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

#[derive(Resource, Default)]
pub(super) struct TestEngines {
    pub(super) tts: Option<Arc<dyn TtsEngine>>,
    pub(super) stt: Option<Arc<dyn SttEngine>>,
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(Voice::new(test_runtime()));
    world.insert_resource(TestEngines {
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

#[test]
fn out_of_order_synthesis_is_released_in_sequence_order() {
    let mut emitter = Emitter::new("u".into(), 7);
    // The second sentence finishes first: nothing is sent yet.
    assert!(emitter.offer(1, Ok(vec![2, 0])).is_empty());
    let events = emitter.offer(0, Ok(vec![1, 0]));
    assert_eq!(chunks(&events), [0, 1]);
    assert!(events.iter().all(|event| matches!(
        &event.event,
        Some(w::audio_output_event::Event::Chunk(chunk)) if chunk.is_final_chunk
    )));
    assert_eq!(finished(&emitter.finish(2)), Some(2));
}

#[test]
fn a_skipped_sentence_unblocks_the_next_one() {
    let mut emitter = Emitter::new("u".into(), 7);
    let events = emitter.offer(0, Err("timed out".into()));
    assert!(matches!(
        events.as_slice(),
        [w::AudioOutputEvent { event: Some(w::audio_output_event::Event::Skipped(skipped)), .. }]
            if skipped.sequence_no == 0
    ));
    assert_eq!(chunks(&emitter.offer(1, Ok(vec![0, 0]))), [1]);
    assert_eq!(finished(&emitter.finish(2)), Some(2));
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
fn a_voice_intent_is_conservative() {
    for text in [
        "はい",
        "いいよ",
        "お願いします",
        "実行して",
        "どうぞ",
        "yes",
        "OK",
        "Approve it",
    ] {
        assert_eq!(
            approval_intent(text),
            Some(ApprovalDecision::Approve),
            "{text}"
        );
    }
    for text in [
        "いいえ",
        "だめ",
        "やめて",
        "やらない",
        "キャンセル",
        "no",
        "deny",
        "cancel",
    ] {
        assert_eq!(
            approval_intent(text),
            Some(ApprovalDecision::Deny),
            "{text}"
        );
    }
    // A negative word wins when both kinds appear, and an unknown sentence is
    // never an approval.
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
fn audio_bound_to_a_moved_activation_is_discarded() {
    let (_root, store) = store_with_client();
    let mut world = World::new();
    world.insert_resource(store.clone());
    let chunk = w::AudioInputChunk {
        client_id: "c1".into(),
        activation_revision: 1,
        ..Default::default()
    };
    let binding = bind_input(&world, &chunk).unwrap();
    assert!(binding.accepts(&chunk) && is_still_active(&world, &binding));

    store.activate("ene", Some("c1"), 1).unwrap();
    assert!(!is_still_active(&world, &binding));
    let moved = w::AudioInputChunk {
        activation_revision: 2,
        ..chunk.clone()
    };
    assert!(!binding.accepts(&moved));
    assert!(matches!(
        bind_input(&world, &chunk),
        Err(HostError::Conflict(_))
    ));
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

#[test]
fn a_host_without_a_connection_reports_the_capability_as_missing() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    let mut world = World::new();
    world.insert_resource(store);
    world.insert_resource(Credentials::default());
    assert_eq!(tts_source(&world).err(), Some(SetupError::Missing));
    assert_eq!(stt_source(&world).err(), Some(SetupError::Missing));
}

#[test]
fn the_stt_wav_header_names_the_wire_format() {
    let wav = wav_wrap(&[0u8; 8], 16_000);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
    assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
    assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
    assert_eq!(wav.len(), 52);
}

/// A stereo WAV in the 16-bit shape the TTS endpoints return.
fn stereo_wav(frames: &[(i16, i16)]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(frames.len() * 4);
    for (left, right) in frames {
        pcm.extend_from_slice(&left.to_le_bytes());
        pcm.extend_from_slice(&right.to_le_bytes());
    }
    let mut wav = wav_wrap(&pcm, SAMPLE_RATE);
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

#[test]
fn truncated_wav_chunks_are_rejected() {
    let wav = wav_wrap(&[0u8; 8], SAMPLE_RATE);
    assert!(parse_wav(&wav).is_ok());
    for cut in [0, 4, 10, 12, 20, 30, 44] {
        assert!(parse_wav(&wav[..cut]).is_err(), "cut at {cut}");
    }
    // The fmt chunk claims more bytes than the file carries.
    let mut lying = wav.clone();
    lying[16..20].copy_from_slice(&64u32.to_le_bytes());
    assert!(parse_wav(&lying).is_err());
    // The data chunk claims more bytes than the file carries.
    let mut lying = wav.clone();
    lying[40..44].copy_from_slice(&1_000u32.to_le_bytes());
    assert!(parse_wav(&lying).is_err());
    // The fmt chunk is too small for the PCM fields it must carry.
    let mut small = wav.clone();
    small[16..20].copy_from_slice(&14u32.to_le_bytes());
    assert!(parse_wav(&small).is_err());
}

#[tokio::test]
async fn plain_http_and_private_targets_are_rejected() {
    for allowed in [
        "http://127.0.0.1:5002",
        "http://[::1]:5002",
        "http://localhost:5002",
        "https://127.0.0.1",
    ] {
        assert!(
            net::guard_endpoint(allowed, "TTS").await.is_ok(),
            "{allowed}"
        );
    }
    for rejected in [
        "http://example.com",
        "https://10.0.0.1",
        "https://192.168.1.4",
        "https://169.254.169.254",
        "https://[fe80::1]",
        "https://[::ffff:10.0.0.1]",
        "ftp://127.0.0.1",
    ] {
        assert!(
            net::guard_endpoint(rejected, "TTS").await.is_err(),
            "{rejected}"
        );
    }
    let endpoint = |endpoint: &str| Endpoint {
        adapter: "openai".into(),
        endpoint: endpoint.into(),
        options_schema: String::new(),
        options: String::new(),
    };
    assert!(base_url(&endpoint("http://example.com"), "TTS").is_err());
    // The setup only validates the scheme; the request guard owns the address.
    assert_eq!(
        base_url(&endpoint("https://10.0.0.1"), "TTS").unwrap(),
        "https://10.0.0.1"
    );
    assert_eq!(
        base_url(&endpoint("http://127.0.0.1:5002/"), "TTS").unwrap(),
        "http://127.0.0.1:5002"
    );
}

#[tokio::test]
async fn fake_engines_drive_both_traits_without_a_network() {
    let pcm = synthesize(&FakeTts { fail: false }, "ab".into(), String::new())
        .await
        .unwrap();
    assert_eq!(pcm.len(), 4);
    assert!(
        synthesize(&FakeTts { fail: true }, "あ".into(), String::new())
            .await
            .is_err()
    );
    assert_eq!(
        FakeStt.transcribe(vec![0; 480], SAMPLE_RATE).await.unwrap(),
        "480 bytes"
    );
}

#[tokio::test]
async fn the_worker_orders_audio_and_finishes_the_utterance() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1", 1);
    voice.begin("ene", Arc::new(FakeTts { fail: false }), String::new());
    let utterance_id = {
        let state = voice.lock();
        state.owners["ene"].utterance.as_ref().unwrap().id.clone()
    };
    {
        let mut state = voice.lock();
        let work = state.works.get_mut(&utterance_id).unwrap();
        // The second sentence is queued first: the ordering gate still emits
        // sequence 0 before sequence 1.
        for sequence_no in [1u32, 0] {
            work.pending.push_back(Sentence {
                sequence_no,
                text: format!("s{sequence_no}"),
                source: Arc::new(FakeTts { fail: false }),
                voice: String::new(),
            });
        }
        work.emitter.finish(2);
        voice.pump(&mut state, &utterance_id);
    }
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

mod wave_format {
    use super::super::*;

    #[test]
    fn a_non_pcm_wav_is_rejected() {
        let mut wav = wav_wrap(&[0u8; 8], 24_000);
        // The fmt chunk payload starts at offset 20; AudioFormat is first.
        wav[20..22].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_wav(&wav).is_err());
    }

    #[test]
    fn a_float_wav_is_rejected_as_non_pcm() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = std::io::Cursor::new(Vec::new());
        let mut writer = hound::WavWriter::new(&mut wav, spec).unwrap();
        writer.write_sample(0.0f32).unwrap();
        writer.finalize().unwrap();
        let error = parse_wav(&wav.into_inner()).unwrap_err();
        assert!(error.contains("not PCM"), "{error}");
    }
}

mod resampling {
    use super::super::*;

    #[test]
    fn a_48k_tone_is_converted_to_the_24k_wire_rate() {
        let samples: Vec<i16> = (0..4_800)
            .map(|index| ((index as f32 * 0.1).sin() * 10_000.0) as i16)
            .collect();
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let out = resample_mono(&pcm, 48_000, 24_000).unwrap();
        assert_eq!(out.len() / 2, 2_400);
        let samples: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        let first = samples
            .iter()
            .position(|sample| sample.abs() > 100)
            .expect("the tone is audible");
        assert!(first < 480, "the output starts with {first} silent samples");
        assert!(
            samples[samples.len() * 9 / 10..]
                .iter()
                .any(|sample| sample.abs() > 100),
            "the tail was flushed away"
        );
    }
}
