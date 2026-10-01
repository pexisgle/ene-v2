//! Optional native telemetry; no image capture or GPU waits. Only timestamp
//! staging buffers (16 bytes per completed SceneTimer sample) are read here.
//!
//! Integration notes (smithay-client-toolkit **0.21.1**, not older macros):
//! * Add `mod telemetry;` to main, create `Recorder::shared(backend)` once and
//!   share it with native State / render systems. Recording is gated by
//!   `ENE_AVATAR_PROFILE=1`; verification additionally requires
//!   `ENE_AVATAR_VERIFY=1`. Call `emit_summary()` periodically and at shutdown.
//! * State needs `presentation: PresentationTimeState` initialized using
//!   `PresentationTimeState::bind(globals, qh)` and the shared recorder. Implement
//!   `PresentationTimeHandler::presentation_time_state` returning that field.
//!   The existing `smithay_client_toolkit::delegate_dispatch2!(State)` handles
//!   both presentation interfaces. Do NOT add a delegate_presentation_time macro.
//! * Just before the commit associated with a new buffer (including the commit
//!   made by wgpu SurfaceTexture::present), call
//!   `state.presentation.feedback(state.layer.wl_surface(), qh)`. On Ok(feedback),
//!   call `presentation_requested(feedback.id().protocol_id(), frame_id)` (import
//!   `wayland_client::Proxy`). Missing global: call
//!   `presentation_unavailable("wp_presentation global unavailable")` once.
//!   Do not request feedback on input-region/property-only commits.
//! * Implement the trait's `presented` callback with arguments:
//!   `&mut self, &Connection, &QueueHandle<Self>, &WpPresentationFeedback,
//!   &WlSurface, Vec<WlOutput>, PresentTime, u32 /* refresh ns */, u64 /* seq */,
//!   WEnum<wp_presentation_feedback::Kind>`. Forward to `Recorder::presented`
//!   using proxy ID, `time`, refresh, seq, and `flags.into()` (u32). Implement
//!   `discarded(&mut self, &Connection, &QueueHandle<Self>,
//!   &WpPresentationFeedback, &WlSurface)` and forward the proxy ID. Protocol
//!   types are in `smithay_client_toolkit::reexports::protocols::wp::
//!   presentation_time::client`; time/state/trait in `presentation_time`.
//!   presented/discarded are terminal feedback events. Forget feedback mappings
//!   when a surface is destroyed using `reset_presentation()`. If State owns an
//!   ObjectId -> (frame_id, Instant) map, use record_presentation/record_discarded
//!   directly instead of the recorder's optional u32 mapping helpers. Clear the
//!   State map on surface recreation. Never mix the two tracking paths.
//! * frame_enqueued(frame_id, original_instant) retains that instant and samples
//!   CLOCK_MONOTONIC between two Instant reads on Linux, even if registration is
//!   delayed until frame consumption. presented derives enqueue-to-display
//!   bounds from wp_presentation ONLY for that same clock. The estimate is the
//!   midpoint, uncertainty the maximum distance to either bound. The bounds
//!   include the sampling bracket and clock resolution, conditional on the
//!   explicit mapping assumptions; they do not bound compositor timestamp error
//!   or physical photon timing. Other clocks/missing mappings yield None plus a
//!   reason. Enqueue-to-callback remains a separate, less precise upper bound.
//! * Measure CPU stages with Instant around actual work. Record an absent stage
//!   as None, not zero. Supply actual logical dimensions and effective scale.
//!   GPU timestamps must come from asynchronously resolved timestamp queries:
//!   scene, mask, and present spans in the SAME GPU clock with timestamp period
//!   from the device. Use scene_render_batch for buffered RenderGraph timestamps;
//!   the legacy scene field serializes as scene_queue_elapsed. Neither the GPU
//!   present pass nor a queue span is compositor display latency.
//!   None + unavailable_reason represents unsupported/missing samples.
//!   Never synchronously wait for a query or copy an image for production metrics.
//! * Verification is a separate process: compare the actual submitted logical
//!   input region with rendered alpha ground truth on the same frame/scale.
//!   Explicit TP/FP/FN/TN counts go to record_verification. Only that opt-in
//!   verifier may read back alpha; metrics themselves never initiate readback.
//!   Compare final regions after physical-to-logical coverage conversion; record
//!   the reference rule (threshold/coverage), backend, frame and dimensions.

use bevy::render::renderer::{PendingCommandBuffers, RenderDevice, RenderQueue};
use serde::Serialize;
use smithay_client_toolkit::presentation_time::PresentTime;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Write},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const METRIC_PREFIX: &str = "ENE_AVATAR_METRIC ";
pub const SCHEMA_VERSION: u32 = 1;
const MAX_PENDING: usize = 4096;
const MAX_INTERVALS: usize = 4096;
pub type SharedRecorder = Arc<Mutex<Recorder>>;

/// Durations cover CPU work only, not GPU execution or compositor display.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuFrameDurations {
    pub frame_id: u64,
    pub logical_width: u32,
    pub logical_height: u32,
    pub scale: f64,
    pub total_ns: u64,
    pub motion_ns: Option<u64>,
    pub update_ns: Option<u64>,
    pub scene_ns: Option<u64>,
    pub mask_ns: Option<u64>,
    pub input_region_ns: Option<u64>,
    pub submit_ns: Option<u64>,
    pub decode_ns: Option<u64>,
    /// Projection/decode, producer setup, and exact Wayland region registration.
    pub coverage_ns: Option<u64>,
}

/// Already converted timings from GPU frame transport or presentation queries.
/// None means unavailable, never an assumed zero or a fabricated timestamp span.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GpuDurations {
    pub frame_id: u64,
    /// Genuine render-batch GPU duration; only buffered scene timestamps belong here.
    pub scene_ns: Option<u64>,
    pub scene_queue_elapsed_ns: Option<u64>,
    pub mask_ns: Option<u64>,
    pub copy_mask_ns: Option<u64>,
    pub present_ns: Option<u64>,
    /// CPU monotonic elapsed time from immutable enqueue to present-call entry.
    /// This is transport latency, not compositor display latency or GPU work.
    pub transport_latency_ns: Option<u64>,
    /// Actual compact-mask/verification bytes; excludes timestamp staging bytes.
    pub readback_bytes: u64,
    pub timestamp_readback_bytes: u64,
    pub overflow_retries: u32,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct GpuTimestampSpan {
    pub start_ticks: u64,
    pub end_ticks: u64,
}
impl GpuTimestampSpan {
    /// Reversed samples are unavailable (do not fabricate wraparound timing).
    pub fn duration_ns(&self, timestamp_period_ns: f64) -> Option<f64> {
        let ticks = self.end_ticks.checked_sub(self.start_ticks)?;
        let duration = ticks as f64 * timestamp_period_ns;
        (timestamp_period_ns.is_finite() && timestamp_period_ns > 0.0 && duration.is_finite())
            .then_some(duration)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GpuTimestampSample {
    pub frame_id: u64,
    pub timestamp_period_ns: f64,
    #[serde(rename = "scene_queue_elapsed")]
    pub scene: Option<GpuTimestampSpan>,
    /// Start/render/end command buffers submitted in the same RenderGraph batch.
    pub scene_render_batch: Option<GpuTimestampSpan>,
    pub mask: Option<GpuTimestampSpan>,
    #[serde(rename = "present_queue_elapsed")]
    pub present: Option<GpuTimestampSpan>,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PresentationSample {
    pub frame_id: Option<u64>,
    pub clock_id: u32,
    pub presentation_ns: Option<u64>,
    pub refresh_ns: u32,
    pub sequence: u64,
    pub flags: u32,
    /// Same-process monotonic duration until callback receipt; not display latency.
    pub callback_latency_ns: Option<u64>,
    /// Enqueue -> callback receipt, kept separate from clock-aligned display latency.
    pub enqueue_to_callback_ns: Option<u64>,
    pub enqueue_to_display_ns_estimate: Option<u64>,
    pub enqueue_to_display_ns_lower_bound: Option<u64>,
    pub enqueue_to_display_ns_upper_bound: Option<u64>,
    /// Radius around the midpoint estimate, not a statistical confidence interval.
    pub enqueue_to_display_ns_uncertainty: Option<u64>,
    pub enqueue_clock_mapping: Option<EnqueueClockMapping>,
    pub display_latency_unavailable_reason: Option<&'static str>,
}

/// Mapping of an enqueue Instant to a Linux CLOCK_MONOTONIC interval.
#[derive(Debug, Clone, Serialize)]
pub struct EnqueueClockMapping {
    pub clock_id: u32,
    pub enqueue_clock_lower_ns: u64,
    pub enqueue_clock_upper_ns: u64,
    pub calibration_window_ns: u64,
    pub clock_resolution_ns: u64,
    /// Age at the beginning of registration's clock-sampling bracket.
    pub registration_age_ns: u64,
    pub assumptions: &'static str,
}

impl EnqueueClockMapping {
    #[cfg(target_os = "linux")]
    fn from_bracket(
        enqueued_at: Instant,
        before: Instant,
        after: Instant,
        clock_ns: u64,
        resolution_ns: u64,
    ) -> Result<Self, &'static str> {
        let age_before = before
            .checked_duration_since(enqueued_at)
            .ok_or("enqueue Instant is later than registration bracket")?;
        let age_after = after
            .checked_duration_since(enqueued_at)
            .ok_or("enqueue Instant is later than registration bracket")?;
        let width = after
            .checked_duration_since(before)
            .ok_or("Instant clock moved backwards during calibration")?;
        let age_before =
            u64::try_from(age_before.as_nanos()).map_err(|_| "enqueue age overflow")?;
        let age_after = u64::try_from(age_after.as_nanos()).map_err(|_| "enqueue age overflow")?;
        let lower = clock_ns
            .checked_sub(age_after)
            .and_then(|n| n.checked_sub(resolution_ns))
            .ok_or("enqueue clock mapping underflow")?;
        let upper = clock_ns
            .checked_sub(age_before)
            .and_then(|n| n.checked_add(resolution_ns))
            .ok_or("enqueue clock mapping overflow")?;
        Ok(Self {
            clock_id: rustix::time::ClockId::Monotonic as u32,
            enqueue_clock_lower_ns: lower,
            enqueue_clock_upper_ns: upper,
            calibration_window_ns: duration_ns(width),
            clock_resolution_ns: resolution_ns,
            registration_age_ns: age_before,
            assumptions: "Linux std::Instant and CLOCK_MONOTONIC advance at the same rate; the clock_gettime sample occurs inside the Instant bracket; the compositor's CLOCK_MONOTONIC timestamp shares the local epoch/rate; enqueue precedes display. Bounds cover registration bracket and clock resolution, not compositor timestamp error, clock drift, or physical photon timing.",
        })
    }

    fn display_latency_bounds(
        &self,
        clock_id: u32,
        presentation_ns: Option<u64>,
    ) -> Result<(u64, u64), &'static str> {
        if clock_id != self.clock_id {
            return Err("presentation clock differs from sampled CLOCK_MONOTONIC");
        }
        let presentation_ns = presentation_ns.ok_or("invalid or missing presentation timestamp")?;
        let upper = presentation_ns
            .checked_sub(self.enqueue_clock_lower_ns)
            .ok_or("presentation timestamp precedes enqueue clock bounds")?;
        // The nonnegative intersection is valid under the recorded assumption
        // that this frame was enqueued before it was displayed.
        let lower = presentation_ns.saturating_sub(self.enqueue_clock_upper_ns);
        Ok((lower, upper))
    }
}

struct FrameEnqueue {
    instant: Instant,
    monotonic: Result<EnqueueClockMapping, &'static str>,
}

#[cfg(target_os = "linux")]
fn sample_enqueue_clock(enqueued_at: Instant) -> Result<EnqueueClockMapping, &'static str> {
    use rustix::time::{ClockId, Timespec, clock_getres, clock_gettime};
    fn timespec_ns(time: Timespec) -> Option<u64> {
        let seconds = u64::try_from(time.tv_sec).ok()?;
        let nanos = u64::try_from(time.tv_nsec).ok()?;
        if nanos >= 1_000_000_000 {
            return None;
        }
        seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
    }
    let before = Instant::now();
    let clock = clock_gettime(ClockId::Monotonic);
    let after = Instant::now();
    let resolution = clock_getres(ClockId::Monotonic);
    let clock_ns = timespec_ns(clock).ok_or("invalid CLOCK_MONOTONIC sample")?;
    let resolution_ns = timespec_ns(resolution)
        .filter(|n| *n > 0)
        .ok_or("invalid CLOCK_MONOTONIC resolution")?;
    EnqueueClockMapping::from_bracket(enqueued_at, before, after, clock_ns, resolution_ns)
}

#[cfg(not(target_os = "linux"))]
fn sample_enqueue_clock(_: Instant) -> Result<EnqueueClockMapping, &'static str> {
    Err("enqueue clock mapping currently supports Linux CLOCK_MONOTONIC only")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PresentationStats {
    pub requested: u64,
    pub presented: u64,
    pub discarded: u64,
    pub pending: usize,
    pub abandoned: u64,
    pub untracked_callbacks: u64,
    pub clock_resets: u64,
    pub interval_count: u64,
    pub fps: Option<f64>,
    pub interval_mean_ns: Option<f64>,
    pub interval_p50_ns: Option<u64>,
    pub interval_p95_ns: Option<u64>,
    /// Percentiles use the latest bounded window; mean/FPS cover all intervals.
    pub percentile_window_samples: usize,
    pub unavailable_reason: Option<String>,
}

/// Positive = included in the submitted logical input region. Ground truth is
/// rendered alpha under the explicitly described reference rule.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationSample {
    pub frame_id: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub reference_rule: String,
    pub true_positives: u64,
    pub false_positive: u64,
    pub false_negative: u64,
    pub true_negatives: u64,
}
impl VerificationSample {
    pub fn is_complete(&self) -> bool {
        self.true_positives
            .checked_add(self.false_positive)
            .and_then(|n| n.checked_add(self.false_negative))
            .and_then(|n| n.checked_add(self.true_negatives))
            == Some(u64::from(self.width) * u64::from(self.height))
            && self.width > 0
            && self.height > 0
            && self.scale.is_finite()
            && self.scale > 0.0
            && !self.reference_rule.is_empty()
    }
}

pub fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

pub struct Recorder {
    enabled: bool,
    verify: bool,
    backend: String,
    started: Instant,
    cpu_frames: u64,
    gpu_samples: u64,
    verification_samples: u64,
    pending: BTreeMap<u32, (u64, Instant)>,
    enqueued: BTreeMap<u64, FrameEnqueue>,
    stats: PresentationStats,
    last_presented: Option<(u32, u64)>,
    interval_sum_ns: u128,
    intervals: VecDeque<u64>,
}
impl Recorder {
    pub fn new(backend: impl Into<String>) -> Self {
        Self::with_options(
            backend,
            std::env::var("ENE_AVATAR_PROFILE").as_deref() == Ok("1"),
            std::env::var("ENE_AVATAR_VERIFY").as_deref() == Ok("1"),
        )
    }
    pub fn shared(backend: impl Into<String>) -> SharedRecorder {
        Arc::new(Mutex::new(Self::new(backend)))
    }
    pub fn with_options(backend: impl Into<String>, enabled: bool, verify: bool) -> Self {
        Self {
            enabled,
            verify,
            backend: backend.into(),
            started: Instant::now(),
            cpu_frames: 0,
            gpu_samples: 0,
            verification_samples: 0,
            pending: BTreeMap::new(),
            enqueued: BTreeMap::new(),
            stats: PresentationStats::default(),
            last_presented: None,
            interval_sum_ns: 0,
            intervals: VecDeque::new(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn verification_enabled(&self) -> bool {
        self.verify
    }

    fn emit(&self, kind: &str, sample: impl Serialize) {
        if !self.enabled {
            return;
        }
        #[derive(Serialize)]
        struct Envelope<'a, T> {
            schema_version: u32,
            kind: &'a str,
            unix_ns: u64,
            elapsed_ns: u64,
            pid: u32,
            backend: &'a str,
            verify: bool,
            sample: T,
        }
        let record = Envelope {
            schema_version: SCHEMA_VERSION,
            kind,
            unix_ns: duration_ns(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            elapsed_ns: duration_ns(self.started.elapsed()),
            pid: std::process::id(),
            backend: &self.backend,
            verify: self.verify,
            sample,
        };
        if let Ok(json) = serde_json::to_string(&record) {
            // One stderr lock keeps metrics intact with tracing/log writers.
            let _ = writeln!(io::stderr().lock(), "{METRIC_PREFIX}{json}");
        }
    }
    pub fn record_cpu_frame(&mut self, sample: CpuFrameDurations) {
        if !self.enabled {
            return;
        }
        self.cpu_frames += 1;
        self.emit("cpu_frame", sample);
    }
    pub fn record_gpu_timestamps(&mut self, sample: GpuTimestampSample) {
        if !self.enabled {
            return;
        }
        self.gpu_samples += 1;
        self.emit("gpu_timestamps", sample);
    }
    pub fn record_gpu_durations(&mut self, sample: GpuDurations) {
        if !self.enabled {
            return;
        }
        self.gpu_samples += 1;
        self.emit("gpu_durations", sample);
    }
    /// Call after successful frame enqueue with the original enqueue instant,
    /// or when consuming the immutable GPU Frame's timings.enqueued_at. Repeated
    /// registrations preserve that first instant. Stale/dropped frames are bounded.
    pub fn frame_enqueued(&mut self, frame_id: u64, enqueued_at: Instant) {
        if !self.enabled || self.enqueued.contains_key(&frame_id) {
            return;
        }
        if self.enqueued.len() >= MAX_PENDING {
            self.enqueued.pop_first();
        }
        let monotonic = sample_enqueue_clock(enqueued_at);
        self.emit("frame_enqueued", serde_json::json!({
            "frame_id": frame_id,
            "enqueue_elapsed_ns": enqueued_at.checked_duration_since(self.started).map(duration_ns),
            "enqueue_clock_mapping": monotonic.as_ref().ok(),
            "clock_mapping_unavailable_reason": monotonic.as_ref().err(),
        }));
        self.enqueued.insert(
            frame_id,
            FrameEnqueue {
                instant: enqueued_at,
                monotonic,
            },
        );
    }
    pub fn presentation_requested(&mut self, feedback_id: u32, frame_id: u64) {
        if !self.enabled {
            return;
        }
        if self.pending.len() >= MAX_PENDING {
            self.reset_presentation();
        }
        if self
            .pending
            .insert(feedback_id, (frame_id, Instant::now()))
            .is_some()
        {
            self.stats.abandoned += 1;
        }
        self.stats.requested += 1;
        self.stats.unavailable_reason = None;
        self.emit(
            "presentation_requested",
            serde_json::json!({"feedback_id": feedback_id, "frame_id": frame_id}),
        );
    }
    pub fn presentation_unavailable(&mut self, reason: impl Into<String>) {
        if !self.enabled {
            return;
        }
        self.stats.unavailable_reason = Some(reason.into());
        self.emit("presentation_unavailable", &self.stats.unavailable_reason);
    }
    pub fn presented(
        &mut self,
        feedback_id: u32,
        time: PresentTime,
        refresh_ns: u32,
        sequence: u64,
        flags: u32,
    ) {
        if !self.enabled {
            return;
        }
        let tracked = self.pending.remove(&feedback_id);
        if tracked.is_none() {
            self.stats.untracked_callbacks += 1;
        }
        let ns = (time.tv_nsec < 1_000_000_000)
            .then_some(time.tv_nsec)
            .and_then(|n| {
                time.tv_sec
                    .checked_mul(1_000_000_000)?
                    .checked_add(u64::from(n))
            });
        self.record_presentation(PresentationSample {
            frame_id: tracked.map(|p| p.0),
            clock_id: time.clk_id,
            presentation_ns: ns,
            refresh_ns,
            sequence,
            flags,
            callback_latency_ns: tracked.map(|p| duration_ns(p.1.elapsed())),
            ..Default::default()
        });
    }
    /// Use when native State owns the feedback -> frame/Instant mapping.
    pub fn record_presentation(&mut self, mut sample: PresentationSample) {
        if !self.enabled {
            return;
        }
        self.stats.presented += 1;
        sample.enqueue_to_display_ns_estimate = None;
        sample.enqueue_to_display_ns_lower_bound = None;
        sample.enqueue_to_display_ns_upper_bound = None;
        sample.enqueue_to_display_ns_uncertainty = None;
        sample.enqueue_clock_mapping = None;
        sample.display_latency_unavailable_reason =
            Some("original frame enqueue registration unavailable");
        if let Some(enqueued) = sample.frame_id.and_then(|id| self.enqueued.remove(&id)) {
            sample.enqueue_to_callback_ns = Instant::now()
                .checked_duration_since(enqueued.instant)
                .map(duration_ns);
            match enqueued.monotonic {
                Ok(mapping) => {
                    match mapping.display_latency_bounds(sample.clock_id, sample.presentation_ns) {
                        Ok((lower, upper)) => {
                            let estimate = lower + (upper - lower) / 2;
                            sample.enqueue_to_display_ns_estimate = Some(estimate);
                            sample.enqueue_to_display_ns_lower_bound = Some(lower);
                            sample.enqueue_to_display_ns_upper_bound = Some(upper);
                            sample.enqueue_to_display_ns_uncertainty = Some(upper - estimate);
                            sample.display_latency_unavailable_reason = None;
                        }
                        Err(reason) => sample.display_latency_unavailable_reason = Some(reason),
                    }
                    sample.enqueue_clock_mapping = Some(mapping);
                }
                Err(reason) => sample.display_latency_unavailable_reason = Some(reason),
            }
        }
        let ns = sample.presentation_ns;
        if let Some(ns) = ns {
            if let Some((clock, previous)) = self.last_presented {
                if clock == sample.clock_id && ns > previous {
                    let delta = ns - previous;
                    self.stats.interval_count += 1;
                    self.interval_sum_ns += u128::from(delta);
                    if self.intervals.len() == MAX_INTERVALS {
                        self.intervals.pop_front();
                    }
                    self.intervals.push_back(delta);
                } else {
                    self.stats.clock_resets += 1;
                }
            }
            self.last_presented = Some((sample.clock_id, ns));
        } else {
            self.last_presented = None;
            self.stats.clock_resets += 1;
        }
        self.stats.unavailable_reason = None;
        self.emit("presented", sample);
    }
    pub fn discarded(&mut self, feedback_id: u32) {
        if !self.enabled {
            return;
        }
        let tracked = self.pending.remove(&feedback_id);
        if tracked.is_none() {
            self.stats.untracked_callbacks += 1;
        }
        self.record_discarded(tracked.map(|p| p.0));
    }
    pub fn record_discarded(&mut self, frame_id: Option<u64>) {
        if !self.enabled {
            return;
        }
        self.stats.discarded += 1;
        if let Some(frame_id) = frame_id {
            self.enqueued.remove(&frame_id);
        }
        self.emit("discarded", serde_json::json!({"frame_id": frame_id}));
    }
    pub fn reset_presentation(&mut self) {
        self.stats.abandoned += self.pending.len() as u64;
        self.pending.clear();
        self.enqueued.clear();
        self.last_presented = None;
    }
    pub fn presentation_stats(&self) -> PresentationStats {
        let mut stats = self.stats.clone();
        stats.pending = self.pending.len();
        if stats.interval_count > 0 {
            let mean = self.interval_sum_ns as f64 / stats.interval_count as f64;
            stats.interval_mean_ns = Some(mean);
            stats.fps = Some(1_000_000_000.0 / mean);
        }
        let mut values: Vec<_> = self.intervals.iter().copied().collect();
        values.sort_unstable();
        stats.percentile_window_samples = values.len();
        if !values.is_empty() {
            stats.interval_p50_ns = Some(values[(values.len() * 50).div_ceil(100) - 1]);
            stats.interval_p95_ns = Some(values[(values.len() * 95).div_ceil(100) - 1]);
        }
        stats
    }
    /// Returns false if verification is disabled or the confusion matrix is invalid.
    pub fn record_verification(&mut self, sample: VerificationSample) -> bool {
        if !self.enabled || !self.verify || !sample.is_complete() {
            return false;
        }
        self.verification_samples += 1;
        self.emit("verification", sample);
        true
    }
    pub fn emit_summary(&self) {
        self.emit(
            "summary",
            serde_json::json!({
                "cpu_frames": self.cpu_frames, "gpu_samples": self.gpu_samples,
                "verification_samples": self.verification_samples,
                "presentation": self.presentation_stats(),
            }),
        );
    }
}

pub const SCENE_TIMER_SLOTS: usize = 3;
const SCENE_TIMESTAMP_BYTES: u64 = 16;

/// A sample retains the original frame identity even when mapping finishes later.
/// With buffered=true the span covers scene work inside one RenderGraph batch.
/// Otherwise it brackets separate queue submissions and includes intervening
/// uploads/other work and idle time. It is not a sum of isolated render passes.
#[derive(Debug, Clone, Serialize)]
pub struct SceneTimestampSample {
    pub frame_id: u64,
    pub span: GpuTimestampSpan,
    pub timestamp_period_ns: f64,
    pub timestamp_readback_bytes: u64,
    /// True only for begin_buffered/finish_buffered/map_after_submit.
    pub buffered: bool,
}

enum ScenePending {
    Started(u64, bool),
    ReadyToMap(u64, bool),
    Mapping(u64, bool, Mutex<mpsc::Receiver<Result<(), String>>>),
}
struct SceneSlot {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    staging: wgpu::Buffer,
    pending: Option<ScenePending>,
}

/// Three reusable query sets and staging buffers, no image readback, no waits.
///
/// RenderWorld integration (SceneTimer implements Resource):
/// ```ignore
/// // RenderGraph Begin, before Render:
/// let token = timer.begin_buffered(frame_id, &mut pending);
/// // After Render and its deferred flush, before Submit:
/// if let Some(token) = token { timer.finish_buffered(token, &mut pending); }
/// // RenderGraph Finish, AFTER Submit:
/// if let Some(token) = token { timer.map_after_submit(token); }
/// // Later, using the same original frame_id (never relabel as current frame):
/// for sample in timer.poll() {
///     metrics.lock().unwrap().record_gpu_timestamps(GpuTimestampSample {
///         frame_id: sample.frame_id, timestamp_period_ns: sample.timestamp_period_ns,
///         scene_render_batch: Some(sample.span), ..Default::default()
///     });
/// }
/// ```
/// Call poll every event-loop iteration, including when no new frame is due.
/// begin_buffered returns None if profiling is off, a feature is missing, or all
/// slots are occupied. No in-flight slot is overwritten. Errors are data returned
/// by take_errors, not stderr logs. No additional module declaration is needed.
#[derive(bevy::prelude::Resource)]
pub struct SceneTimer {
    device: RenderDevice,
    #[cfg(test)]
    queue: RenderQueue,
    slots: Vec<SceneSlot>,
    period_ns: f64,
    unavailable_reason: Option<String>,
    errors: Vec<String>,
    pub skipped_frames: u64,
}
impl SceneTimer {
    pub fn new(device: RenderDevice, queue: RenderQueue) -> Self {
        let enabled = std::env::var("ENE_AVATAR_PROFILE").as_deref() == Ok("1");
        Self::with_enabled(device, queue, enabled)
    }
    pub fn with_enabled(device: RenderDevice, queue: RenderQueue, enabled: bool) -> Self {
        let features =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let period_ns = f64::from(queue.get_timestamp_period());
        let reason = if !enabled {
            Some("profiling disabled".into())
        } else if !device.features().contains(features) {
            Some("TIMESTAMP_QUERY_INSIDE_ENCODERS unavailable on enabled device".into())
        } else if !period_ns.is_finite() || period_ns <= 0.0 {
            Some("invalid device timestamp period".into())
        } else {
            None
        };
        let slots = if reason.is_none() {
            let raw = device.wgpu_device();
            (0..SCENE_TIMER_SLOTS)
                .map(|_| SceneSlot {
                    queries: raw.create_query_set(&wgpu::QuerySetDescriptor {
                        label: Some("avatar scene timing"),
                        ty: wgpu::QueryType::Timestamp,
                        count: 2,
                    }),
                    resolve: raw.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("avatar scene timestamp resolve"),
                        size: SCENE_TIMESTAMP_BYTES,
                        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    }),
                    staging: raw.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("avatar scene timestamp staging"),
                        size: SCENE_TIMESTAMP_BYTES,
                        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    }),
                    pending: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            device,
            #[cfg(test)]
            queue,
            slots,
            period_ns,
            unavailable_reason: reason,
            errors: Vec::new(),
            skipped_frames: 0,
        }
    }
    pub fn available(&self) -> bool {
        self.unavailable_reason.is_none()
    }
    #[allow(dead_code)] // Optional integration hook for an explicit unavailable metric.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }
    #[cfg(test)]
    fn begin(&mut self, frame_id: u64) -> Option<usize> {
        if !self.available() {
            return None;
        }
        let Some(index) = self.slots.iter().position(|slot| slot.pending.is_none()) else {
            self.skipped_frames += 1;
            return None;
        };
        let slot = &mut self.slots[index];
        let mut encoder =
            self.device
                .wgpu_device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("avatar scene begin"),
                });
        encoder.write_timestamp(&slot.queries, 0);
        self.queue.submit([encoder.finish()]);
        slot.pending = Some(ScenePending::Started(frame_id, false));
        Some(index)
    }
    /// RenderGraph Begin: append start command buffer before the Render set.
    /// No queue submit. Finish after Render (including deferred command flush)
    /// and before Submit; map only in Finish, after Submit has consumed the batch.
    pub fn begin_buffered(
        &mut self,
        frame_id: u64,
        pending: &mut PendingCommandBuffers,
    ) -> Option<usize> {
        if !self.available() {
            return None;
        }
        let Some(index) = self.slots.iter().position(|slot| slot.pending.is_none()) else {
            self.skipped_frames += 1;
            return None;
        };
        let slot = &mut self.slots[index];
        let mut encoder =
            self.device
                .wgpu_device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("avatar render batch begin"),
                });
        encoder.write_timestamp(&slot.queries, 0);
        // Flush pre-existing deferred encoders before start so their work is not
        // accidentally appended after our end by PendingCommandBuffers::take.
        let earlier = pending.take();
        pending.push(earlier);
        pending.push([encoder.finish()]);
        slot.pending = Some(ScenePending::Started(frame_id, true));
        Some(index)
    }
    pub fn finish_buffered(&mut self, token: usize, pending: &mut PendingCommandBuffers) {
        let Some(slot) = self.slots.get_mut(token) else {
            return;
        };
        let Some(ScenePending::Started(frame_id, true)) = slot.pending.as_ref() else {
            return;
        };
        let frame_id = *frame_id;
        let mut encoder =
            self.device
                .wgpu_device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("avatar render batch finish"),
                });
        encoder.write_timestamp(&slot.queries, 1);
        encoder.resolve_query_set(&slot.queries, 0..2, &slot.resolve, 0);
        encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.staging, 0, SCENE_TIMESTAMP_BYTES);
        // Preserve both finished buffers and deferred encoders ahead of end.
        let render_buffers = pending.take();
        pending.push(render_buffers);
        pending.push([encoder.finish()]);
        slot.pending = Some(ScenePending::ReadyToMap(frame_id, true));
    }
    #[cfg(test)]
    fn finish(&mut self, token: usize) {
        let Some(slot) = self.slots.get_mut(token) else {
            return;
        };
        let Some(ScenePending::Started(frame_id, false)) = slot.pending.as_ref() else {
            return;
        };
        let frame_id = *frame_id;
        let mut encoder =
            self.device
                .wgpu_device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("avatar scene finish"),
                });
        encoder.write_timestamp(&slot.queries, 1);
        encoder.resolve_query_set(&slot.queries, 0..2, &slot.resolve, 0);
        encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.staging, 0, SCENE_TIMESTAMP_BYTES);
        self.queue.submit([encoder.finish()]);
        slot.pending = Some(ScenePending::ReadyToMap(frame_id, false));
        self.map_after_submit(token);
    }
    /// RenderGraph Finish only: calling this before Submit would map a buffer
    /// referenced by an unsubmitted encoder and invalidate submission.
    pub fn map_after_submit(&mut self, token: usize) {
        let Some(slot) = self.slots.get_mut(token) else {
            return;
        };
        let Some(ScenePending::ReadyToMap(frame_id, buffered)) = slot.pending.as_ref() else {
            return;
        };
        let (frame_id, buffered) = (*frame_id, *buffered);
        // A single completion per slot, bounded storage and nonblocking callback.
        let (sender, receiver) = mpsc::sync_channel(1);
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.try_send(result.map_err(|error| error.to_string()));
            });
        slot.pending = Some(ScenePending::Mapping(
            frame_id,
            buffered,
            Mutex::new(receiver),
        ));
    }
    /// Test-only release for the separately submitted legacy queue timer.
    #[cfg(test)]
    fn cancel(&mut self, token: usize) {
        if let Some(slot) = self.slots.get_mut(token)
            && matches!(slot.pending, Some(ScenePending::Started(_, false)))
        {
            slot.pending = None;
        }
    }
    pub fn poll(&mut self) -> Vec<SceneTimestampSample> {
        if !self.available() {
            return Vec::new();
        }
        if let Err(error) = self.device.wgpu_device().poll(wgpu::PollType::Poll) {
            self.errors.push(format!("scene timestamp poll: {error}"));
        }
        let mut samples = Vec::with_capacity(SCENE_TIMER_SLOTS);
        for slot in &mut self.slots {
            let Some(ScenePending::Mapping(frame_id, buffered, receiver)) = slot.pending.as_mut()
            else {
                continue;
            };
            let frame_id = *frame_id;
            let buffered = *buffered;
            let completion = match receiver.get_mut().unwrap().try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("scene timestamp callback disconnected".into())
                }
            };
            slot.pending = None;
            match completion {
                Ok(()) => {
                    let view = slot.staging.slice(..).get_mapped_range();
                    let span = GpuTimestampSpan {
                        start_ticks: u64::from_le_bytes(view[0..8].try_into().unwrap()),
                        end_ticks: u64::from_le_bytes(view[8..16].try_into().unwrap()),
                    };
                    drop(view);
                    slot.staging.unmap();
                    if span.duration_ns(self.period_ns).is_some() {
                        samples.push(SceneTimestampSample {
                            frame_id,
                            span,
                            timestamp_period_ns: self.period_ns,
                            timestamp_readback_bytes: SCENE_TIMESTAMP_BYTES,
                            buffered,
                        });
                    } else {
                        self.errors
                            .push(format!("invalid scene timestamps for frame {frame_id}"));
                    }
                }
                Err(error) => {
                    slot.staging.unmap();
                    self.errors
                        .push(format!("scene timestamp map for frame {frame_id}: {error}"));
                }
            }
        }
        samples
    }
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn delayed_enqueue_registration_maps_clock_bounds_and_rejects_other_clocks() {
        let enqueue = Instant::now();
        let mapping = EnqueueClockMapping::from_bracket(
            enqueue,
            enqueue + Duration::from_nanos(10),
            enqueue + Duration::from_nanos(20),
            1_000,
            1,
        )
        .unwrap();
        assert_eq!(
            (
                mapping.enqueue_clock_lower_ns,
                mapping.enqueue_clock_upper_ns
            ),
            (979, 991)
        );
        assert_eq!(
            (mapping.registration_age_ns, mapping.calibration_window_ns),
            (10, 10)
        );
        let clock = rustix::time::ClockId::Monotonic as u32;
        assert_eq!(
            mapping.display_latency_bounds(clock, Some(1_050)),
            Ok((59, 71))
        );
        assert!(
            mapping
                .display_latency_bounds(rustix::time::ClockId::Realtime as u32, Some(1_050))
                .is_err()
        );
        assert!(mapping.display_latency_bounds(clock, Some(900)).is_err());
        assert!(mapping.display_latency_bounds(clock, None).is_err());
        let mut recorder = Recorder::with_options("gpu", true, false);
        recorder.enqueued.insert(
            5,
            FrameEnqueue {
                instant: enqueue,
                monotonic: Ok(mapping),
            },
        );
        recorder.record_presentation(PresentationSample {
            frame_id: Some(5),
            clock_id: clock,
            presentation_ns: Some(1_050),
            ..Default::default()
        });
        assert!(recorder.enqueued.is_empty());
        recorder.frame_enqueued(6, enqueue);
        recorder.discarded(123); // An untracked feedback must not consume another frame.
        assert!(recorder.enqueued.contains_key(&6));
        recorder.record_discarded(Some(6));
        assert!(recorder.enqueued.is_empty());
        recorder.frame_enqueued(7, enqueue);
        recorder.reset_presentation();
        assert!(recorder.enqueued.is_empty());
    }
    #[test]
    fn scene_timer_pool_is_bounded_asynchronous_and_preserves_frame_ids() {
        use bevy::render::renderer::WgpuWrapper;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = runtime
            .block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                force_fallback_adapter: true,
                ..Default::default()
            }))
            .expect("SceneTimer test requires a Vulkan adapter (lavapipe is sufficient)");
        let requested =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let (device, queue) = runtime
            .block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                required_features: requested & adapter.features(),
                ..Default::default()
            }))
            .unwrap();
        let device: RenderDevice = device.into();
        let queue = RenderQueue(Arc::new(WgpuWrapper::new(queue)));
        let mut off = SceneTimer::with_enabled(device.clone(), queue.clone(), false);
        assert!(!off.available());
        assert!(off.slots.is_empty());
        assert_eq!(off.begin(1), None);
        assert!(off.poll().is_empty());
        let mut timer = SceneTimer::with_enabled(device, queue, true);
        assert!(
            timer.available(),
            "software adapter needs encoder timestamp queries"
        );
        let tokens: Vec<_> = (101..104).map(|id| timer.begin(id).unwrap()).collect();
        assert_eq!(timer.begin(104), None);
        assert_eq!(timer.skipped_frames, 1);
        for token in tokens {
            timer.finish(token);
            timer.finish(token);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut samples = Vec::new();
        while samples.len() < 3 && Instant::now() < deadline {
            samples.extend(timer.poll());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(timer.take_errors().is_empty());
        samples.sort_by_key(|s| s.frame_id);
        assert_eq!(
            samples.iter().map(|s| s.frame_id).collect::<Vec<_>>(),
            vec![101, 102, 103]
        );
        assert!(
            samples
                .iter()
                .all(|s| s.span.duration_ns(s.timestamp_period_ns).is_some()
                    && s.timestamp_readback_bytes == SCENE_TIMESTAMP_BYTES)
        );
        let token = timer.begin(200).unwrap();
        timer.cancel(token);
        let reused = timer.begin(201).unwrap();
        assert_eq!(reused, token);
        timer.cancel(reused);
    }
    #[test]
    fn presentation_tracks_discard_and_never_subtracts_different_clocks() {
        let mut r = Recorder::with_options("gpu", true, false);
        for (id, clock, ns) in [(1, 1, 0), (2, 1, 20_000_000), (3, 2, 0), (4, 2, 40_000_000)] {
            r.presentation_requested(id, u64::from(id));
            r.presented(
                id,
                PresentTime {
                    clk_id: clock,
                    tv_sec: 1,
                    tv_nsec: ns,
                },
                0,
                0,
                0,
            );
        }
        r.presentation_requested(5, 5);
        r.discarded(5);
        r.presentation_requested(6, 6);
        let s = r.presentation_stats();
        assert_eq!(
            (s.requested, s.presented, s.discarded, s.pending),
            (6, 4, 1, 1)
        );
        assert_eq!((s.interval_count, s.clock_resets), (2, 1));
        assert_eq!(s.interval_mean_ns, Some(30_000_000.0));
        assert_eq!(s.interval_p95_ns, Some(40_000_000));
        r.reset_presentation();
        assert_eq!(r.presentation_stats().abandoned, 1);
    }
    #[test]
    fn gpu_timing_rejects_invalid_period_and_reversed_ticks() {
        let span = GpuTimestampSpan {
            start_ticks: 10,
            end_ticks: 30,
        };
        assert_eq!(span.duration_ns(2.5), Some(50.0));
        assert_eq!(span.duration_ns(f64::NAN), None);
        assert_eq!(span.duration_ns(0.0), None);
        assert_eq!(
            GpuTimestampSpan {
                start_ticks: 30,
                end_ticks: 10
            }
            .duration_ns(1.0),
            None
        );
    }
    #[test]
    fn verification_requires_explicit_opt_in_and_complete_counts() {
        let mut s = VerificationSample {
            frame_id: 0,
            width: 2,
            height: 2,
            scale: 1.0,
            reference_rule: "alpha >= 1; any covered physical pixel".into(),
            true_positives: 1,
            false_positive: 1,
            false_negative: 1,
            true_negatives: 1,
        };
        assert!(s.is_complete());
        assert!(!Recorder::with_options("geometry", true, false).record_verification(s.clone()));
        assert!(Recorder::with_options("geometry", true, true).record_verification(s.clone()));
        s.false_positive = u64::MAX;
        assert!(!s.is_complete());
    }
}
