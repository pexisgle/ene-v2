use super::capture::{CaptureSink, EchoSuppressor, PttController, push_captured_with};
use super::playback::{Deck, PlaybackSink, PlayerQueue, fill_with};
use super::resampler::{WIRE_RATE, decode, encode, f32_to_i16, pick_config_from_ranges};
use super::vad::{
    VAD_FRAME_SAMPLES, VAD_HANGOVER_FRAMES, VAD_MAX_ACTIVE_FRAMES, VAD_START_FRAMES, VoiceGate,
};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

fn fill(
    output: &mut [f32],
    channels: usize,
    deck: &Arc<Mutex<Deck>>,
    level: &AtomicU32,
    muted: &AtomicBool,
) {
    fill_with(
        output,
        channels,
        &PlaybackSink { deck, level, muted },
        0.0f32,
        |s| s,
    );
}

fn push_captured(
    input: &[f32],
    channels: usize,
    capacity: usize,
    sink: &Mutex<VecDeque<f32>>,
    muted: &AtomicBool,
    level: &AtomicU32,
) {
    push_captured_with(
        input,
        (channels, capacity),
        &CaptureSink {
            frames: sink,
            muted,
            level,
        },
        |s| s,
    );
}

#[test]
fn voice_gate_stays_closed_on_fluctuating_room_noise() {
    let noise: Vec<f32> = (0..VAD_FRAME_SAMPLES * 150)
        .map(|i| {
            let phase = i as f32;
            let amplitude = 0.008 + 0.020 * (phase * 0.004).sin().abs();
            0.02 + amplitude * (phase * 0.09).sin()
        })
        .collect();
    let white = (0..noise.len())
        .scan(11u32, |state, _| {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Some(0.06 * (*state as f32 / u32::MAX as f32 - 0.5))
        })
        .collect();
    for input in [
        noise.clone(),
        [vec![0.; WIRE_RATE as usize], noise].concat(),
        white,
    ] {
        let mut gate = VoiceGate::new().unwrap();
        for block in input.chunks(729) {
            assert_eq!(gate.process(block).unwrap(), [] as [std::vec::Vec<u8>; 0]);
        }
        assert!(!gate.active);
    }
}

#[test]
fn voice_gate_keeps_preroll_and_separates_utterances_in_one_batch() {
    let mut gate = VoiceGate::new().unwrap();
    let mut audio = Vec::new();
    let mut chunks = Vec::new();
    let pre_roll = [0.01; VAD_FRAME_SAMPLES];
    let speech = [0.04; VAD_FRAME_SAMPLES];
    let silence = [0.0; VAD_FRAME_SAMPLES];
    gate.push_frame(&pre_roll, 1.0, &mut audio, &mut chunks);
    gate.push_frame(&silence, 0.0, &mut audio, &mut chunks);
    assert!(!gate.active, "one high score must not open the gate");

    for _ in 0..2 {
        for _ in 0..VAD_START_FRAMES {
            gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
        }
        assert!(gate.active);
        for _ in 0..=VAD_HANGOVER_FRAMES {
            gate.push_frame(&silence, 0.0, &mut audio, &mut chunks);
        }
        assert!(!gate.active);
    }

    let expected = [
        pre_roll.to_vec(),
        silence.to_vec(),
        speech.repeat(VAD_START_FRAMES as usize),
        silence.repeat(VAD_HANGOVER_FRAMES as usize),
    ]
    .concat();
    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0], encode(&expected));
    assert_eq!(chunks[1], [] as [u8; 0]);
    assert_eq!(chunks[2], encode(&expected[VAD_FRAME_SAMPLES..]));
    assert_eq!(chunks[3], [] as [u8; 0]);
}

#[test]
fn voice_gate_keeps_the_frame_after_a_duration_split() {
    let mut gate = VoiceGate::new().unwrap();
    let mut audio = Vec::new();
    let mut chunks = Vec::new();
    let speech = [0.04; VAD_FRAME_SAMPLES];
    for _ in 0..VAD_START_FRAMES {
        gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
    }
    gate.active_frames = VAD_MAX_ACTIVE_FRAMES - 1;
    gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
    gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
    assert_eq!(
        chunks,
        [
            encode(&speech.repeat(VAD_START_FRAMES as usize + 1)),
            Vec::new()
        ]
    );
    assert_eq!(audio, speech);
}

#[test]
fn stereo_capture_with_dead_channel_preserves_active_channel_amplitude() {
    let sink = Mutex::new(VecDeque::new());
    let muted = AtomicBool::new(false);
    let level = AtomicU32::new(0);
    let stereo_one_channel = [0.02f32, 0.0, -0.02, 0.0, 0.02, 0.0, -0.02, 0.0];
    push_captured(&stereo_one_channel, 2, 64, &sink, &muted, &level);
    let captured: Vec<f32> = sink.lock().iter().copied().collect();
    assert_eq!(captured, [0.02, -0.02, 0.02, -0.02]);
    let rms = f32::from_bits(level.load(Ordering::Relaxed));
    assert!((rms - 0.02).abs() < 1e-6, "{rms}");
}

#[test]
fn wire_pcm_round_trips_through_samples() {
    let samples = [-1., -0.5, 0., 0.5, 1.];
    let pcm = encode(&samples);
    assert_eq!(pcm.len(), samples.len() * 2);
    for (got, want) in decode(&pcm).unwrap().iter().zip(samples) {
        assert!((got - want).abs() < 1e-3, "{got} approximates {want}");
    }
}

#[test]
fn an_odd_byte_count_is_rejected_rather_than_truncated() {
    assert!(decode(&[0, 1, 2]).is_err());
}

#[test]
fn the_played_buffer_reports_its_rms_and_silence() {
    let deck = Arc::new(Mutex::new(Deck::default()));
    deck.lock().pending.extend([0.5f32, -0.5, 0.5, -0.5]);
    let level = AtomicU32::new(0);
    let muted = AtomicBool::new(false);
    let mut output = vec![0.; 4];
    fill(&mut output, 1, &deck, &level, &muted);
    let rms = f32::from_bits(level.load(Ordering::Relaxed));
    assert!((rms - 0.5).abs() < 1e-6, "{rms}");

    let mut output = vec![0.; 4];
    fill(&mut output, 1, &deck, &level, &muted);
    assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
}

#[test]
fn muted_playback_silences_output_and_level_while_advancing_deck() {
    let deck = Arc::new(Mutex::new(Deck::default()));
    {
        let mut guard = deck.lock();
        guard.utterance = "u".into();
        guard.generation = 1;
        guard.pending.extend([0.5f32, -0.5, 0.5, -0.5]);
    }
    let level = AtomicU32::new(0);
    let muted = AtomicBool::new(true);
    let mut output = vec![1.; 4];
    fill(&mut output, 1, &deck, &level, &muted);
    assert_eq!(output, [0., 0., 0., 0.]);
    assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
    assert!(deck.lock().pending.is_empty());
}

#[test]
fn muted_capture_drops_samples_and_clears_rms() {
    let sink = Mutex::new(VecDeque::new());
    let muted = AtomicBool::new(false);
    let level = AtomicU32::new(0);
    push_captured(&[0.5, -0.5, 0.5, -0.5], 2, 64, &sink, &muted, &level);
    assert_eq!(sink.lock().len(), 2);
    assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);

    push_captured(&[0.5, 0.5, -0.5, -0.5], 2, 64, &sink, &muted, &level);
    let rms = f32::from_bits(level.load(Ordering::Relaxed));
    assert!((rms - 0.5).abs() < 1e-6, "{rms}");

    muted.store(true, Ordering::Relaxed);
    push_captured(&[0.9, 0.9], 2, 64, &sink, &muted, &level);
    assert_eq!(sink.lock().len(), 4);
    assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
}

#[test]
fn continuous_alsa_range_picks_wire_rate_and_mono_over_1hz_or_64ch() {
    let ranges = vec![
        cpal::SupportedStreamConfigRange::new(
            64,
            1,
            384_000,
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::F32,
        ),
        cpal::SupportedStreamConfigRange::new(
            1,
            1,
            384_000,
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::F32,
        ),
        cpal::SupportedStreamConfigRange::new(
            2,
            1,
            384_000,
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::I16,
        ),
    ];
    let picked = pick_config_from_ranges(ranges, "output")
        .unwrap()
        .expect("a config is picked");
    assert_eq!(picked.sample_rate(), WIRE_RATE);
    assert_eq!(picked.channels(), 1);
    assert_eq!(picked.sample_format(), cpal::SampleFormat::F32);
}

#[test]
fn i16_only_hardware_range_is_accepted_and_converts_accurately() {
    let ranges = vec![
        cpal::SupportedStreamConfigRange::new(
            2,
            48_000,
            48_000,
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::U8,
        ),
        cpal::SupportedStreamConfigRange::new(
            2,
            48_000,
            48_000,
            cpal::SupportedBufferSize::Unknown,
            cpal::SampleFormat::I16,
        ),
    ];
    let picked = pick_config_from_ranges(ranges, "input")
        .unwrap()
        .expect("i16 fallback is accepted");
    assert_eq!(picked.sample_rate(), 48_000);
    assert_eq!(picked.channels(), 2);
    assert_eq!(picked.sample_format(), cpal::SampleFormat::I16);

    let deck = Arc::new(Mutex::new(Deck::default()));
    {
        let mut guard = deck.lock();
        guard.utterance = "u".into();
        guard.generation = 1;
        guard.pending.extend([0.5f32, -0.5]);
    }
    let level = AtomicU32::new(0);
    let muted = AtomicBool::new(false);
    let mut output_i16 = vec![0i16; 4];
    fill_with(
        &mut output_i16,
        2,
        &PlaybackSink {
            deck: &deck,
            level: &level,
            muted: &muted,
        },
        0i16,
        f32_to_i16,
    );
    assert_eq!(output_i16[0], output_i16[1]);
    assert!(output_i16[0] > 16_000 && output_i16[2] < -16_000);
    assert!((f32::from_bits(level.load(Ordering::Relaxed)) - 0.5).abs() < 1e-4);

    let sink = Mutex::new(VecDeque::new());
    let cap_level = AtomicU32::new(0);
    let cap_muted = AtomicBool::new(false);
    push_captured_with(
        &output_i16,
        (2, 16),
        &CaptureSink {
            frames: &sink,
            muted: &cap_muted,
            level: &cap_level,
        },
        <f32 as cpal::Sample>::from_sample,
    );
    let captured: Vec<f32> = sink.lock().iter().copied().collect();
    assert_eq!(captured.len(), 2);
    assert!((captured[0] - 0.5).abs() < 1e-3);
    assert!((captured[1] + 0.5).abs() < 1e-3);
    assert!((f32::from_bits(cap_level.load(Ordering::Relaxed)) - 0.5).abs() < 1e-3);
}

#[test]
fn b01_stop_current_and_reset_stream_do_not_silence_generation_one() {
    let mut queue = PlayerQueue::new(WIRE_RATE).unwrap();
    let pcm = encode(&[0.25; 64]);
    queue.push("u1", 5, &pcm).unwrap();
    assert!(!queue.deck.lock().pending.is_empty());

    queue.stop_current();
    assert!(queue.deck.lock().pending.is_empty());
    queue.push("u2", 6, &pcm).unwrap();
    assert!(!queue.deck.lock().pending.is_empty());

    queue.flush(6);
    queue.reset_stream();
    queue.push("u3", 1, &pcm).unwrap();
    assert_eq!(queue.deck.lock().generation, 1);
    assert!(!queue.deck.lock().pending.is_empty());
}

#[test]
fn q01_older_generation_chunk_never_overwrites_newer_generation() {
    let mut queue = PlayerQueue::new(WIRE_RATE).unwrap();
    let newer = encode(&[0.5; 32]);
    let older = encode(&[-0.5; 32]);
    queue.push("u3", 3, &newer).unwrap();
    queue.push("u2", 2, &older).unwrap();
    let deck = queue.deck.lock();
    assert_eq!(deck.generation, 3);
    assert_eq!(deck.utterance, "u3");
    assert_eq!(deck.pending.len(), 32);
}

#[test]
fn q02_utterance_switch_resets_filter_and_finish_drains_tail() {
    let mut queue = PlayerQueue::new(48_000).unwrap();
    let partial = encode(&[0.25; 240]);
    queue.push("u1", 1, &partial).unwrap();
    assert!(queue.deck.lock().pending.is_empty());

    let full = encode(&[-0.25; 480]);
    queue.push("u2", 2, &full).unwrap();
    {
        let deck = queue.deck.lock();
        assert_eq!(deck.generation, 2);
        assert!(deck.pending.iter().all(|&s| s <= 0.05));
    }

    let mut queue2 = PlayerQueue::new(48_000).unwrap();
    queue2.push("u3", 1, &partial).unwrap();
    assert!(queue2.deck.lock().pending.is_empty());
    queue2.finish_utterance("u3", 1).unwrap();
    assert!(!queue2.deck.lock().pending.is_empty());
}

#[test]
fn finishing_a_flushed_utterance_keeps_the_new_utterances_resampler_tail() {
    let mut queue = PlayerQueue::new(48_000).unwrap();
    let partial = encode(&[0.25; 240]);
    queue.push("previous", 1, &partial).unwrap();
    queue.flush(1);
    queue.push("current", 2, &partial).unwrap();
    queue.finish_utterance("previous", 1).unwrap();
    queue.finish_utterance("current", 2).unwrap();
    let deck = queue.deck.lock();
    assert_eq!(deck.generation, 2);
    assert!(!deck.pending.is_empty());
}

#[test]
fn f07_ptt_mode_gates_capture_until_active_and_cancel_discards() {
    let mut ptt = PttController::default();
    let idle = ptt.process(&[0.2; 64]);
    assert!(!idle.open && idle.chunks.is_empty());

    ptt.start();
    let active = ptt.process(&[0.25; 64]);
    assert!(active.open);
    assert_eq!(active.chunks.len(), 1);
    assert_ne!(active.chunks[0], [] as [u8; 0]);
    ptt.commit();
    let committed = ptt.process(&[]);
    assert!(!committed.open);
    assert_eq!(committed.chunks, vec![Vec::<u8>::new()]);

    ptt.start();
    let _discarded = ptt.process(&[0.3; 64]);
    ptt.cancel();
    let cancelled = ptt.process(&[0.3; 64]);
    assert!(!cancelled.open && cancelled.chunks.is_empty());
}

#[test]
fn f08_echo_suppressor_blocks_echo_and_detects_barge_in() {
    let mut echo = EchoSuppressor::default();
    for _ in 0..5 {
        assert_eq!(echo.evaluate(true, 0.12, 0.10), (false, false));
    }
    assert_eq!(echo.evaluate(true, 0.45, 0.10), (false, false));
    assert_eq!(echo.evaluate(true, 0.45, 0.10), (false, false));
    assert_eq!(echo.evaluate(true, 0.45, 0.10), (true, true));
    assert_eq!(echo.evaluate(true, 0.45, 0.10), (true, false));
    assert_eq!(echo.evaluate(false, 0.01, 0.0), (true, false));
}
