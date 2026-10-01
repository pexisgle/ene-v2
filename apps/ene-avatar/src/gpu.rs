//! GPU-only frame transport on Bevy's existing device and queue.
//!
//! Call `enqueue` on the main thread after `renderer.app.update()`, using the
//! final `GpuImage` from `RenderApp`. Width/height are physical pixels; runs
//! are logical pixels. `None` requests an exact GPU mask; `Some(runs)` supplies
//! geometry or empty coverage without a mask dispatch/readback. Poll each tick,
//! discard stale generations/sequences in the caller, then draw and install
//! that frame's input region before presenting the returned surface texture.
//!
//! Keep the complete `Frame` alive while displaying/reusing its texture. Its
//! lease prevents ring reuse. A bare cloned `wgpu::Texture` does not hold that
//! lease. Three occupied or leased slots make `enqueue` return false; callers
//! should skip that capture and poll again, never block the render loop.
use crate::mask::Run;
#[cfg(test)]
use bevy::render::renderer::WgpuWrapper;
use bevy::render::renderer::{RenderDevice, RenderQueue};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{
    fmt,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use wgpu::util::DeviceExt;

pub const RING_SIZE: usize = 3;
// Keep in sync with gpu_mask.wgsl. Adjacent pixels share SIMD lanes.
const MASK_PIXELS_PER_WORKGROUP: u32 = 256;
pub const DEFAULT_RUN_CAPACITY: u32 = 4096;
const HEADER_BYTES: u64 = 16;
const RUN_BYTES: u64 = 16;
const TIMESTAMP_BYTES: u64 = 24;

#[derive(Debug, Clone)]
pub struct GpuError(pub String);
impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for GpuError {}

/// CPU timestamps are wall-clock instants; optional GPU durations use the
/// device's timestamp period. Byte counts include every overflow attempt.
#[derive(Debug, Clone)]
pub struct FrameTimings {
    pub enqueued_at: Instant,
    pub submitted_at: Instant,
    pub completed_at: Instant,
    pub decoded_at: Instant,
    pub cpu_submit: Duration,
    pub cpu_decode: Duration,
    pub gpu_copy: Option<Duration>,
    pub gpu_copy_mask: Option<Duration>,
    pub gpu_mask: Option<Duration>,
    pub mask_readback_bytes: u64,
    pub timestamp_readback_bytes: u64,
    pub overflow_retries: u32,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub generation: u64,
    pub sequence: u64,
    pub runs: Vec<Run>,
    #[allow(
        dead_code,
        reason = "verification diagnostics distinguish CPU-supplied coverage from GPU mask readback"
    )]
    pub mask_readback: bool,
    pub timings: FrameTimings,
    _lease: Arc<()>,
}

struct Completion {
    result: Result<(), String>,
    at: Instant,
}
struct Pending {
    width: u32,
    height: u32,
    scale: u32,
    generation: u64,
    sequence: u64,
    cpu_runs: Option<Vec<Run>>,
    receiver: Option<mpsc::Receiver<Completion>>,
    timings: FrameTimings,
    callbacks_remaining: u8,
    completion_error: Option<String>,
    timestamped: bool,
    includes_copy: bool,
}
struct Slot {
    texture: Option<wgpu::Texture>,
    storage: wgpu::Buffer,
    staging: wgpu::Buffer,
    capacity: u32,
    timestamps: Option<Timestamps>,
    pending: Option<Pending>,
    lease: Arc<()>,
}
struct Timestamps {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    staging: wgpu::Buffer,
}
impl Slot {
    fn new(device: &wgpu::Device, capacity: u32) -> Self {
        let bytes = payload_bytes(capacity);
        let timestamps = device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
            .then(|| Timestamps {
                queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("avatar copy/mask timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 3,
                }),
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("avatar timestamp resolve"),
                    size: TIMESTAMP_BYTES,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                staging: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("avatar timestamp staging"),
                    size: TIMESTAMP_BYTES,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
            });
        Self {
            texture: None,
            storage: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("avatar mask storage"),
                size: bytes,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            staging: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("avatar mask staging"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity,
            timestamps,
            pending: None,
            lease: Arc::new(()),
        }
    }
    fn grow(&mut self, device: &wgpu::Device, capacity: u32) {
        // Only the buffers change: texture, lease, metadata, and pending frame
        // remain intact, so overflow retries cannot sample a newer scene.
        let mut replacement = Self::new(device, capacity);
        std::mem::swap(&mut self.storage, &mut replacement.storage);
        std::mem::swap(&mut self.staging, &mut replacement.staging);
        self.capacity = capacity;
    }
}
fn payload_bytes(capacity: u32) -> u64 {
    HEADER_BYTES + u64::from(capacity) * RUN_BYTES
}

pub struct GpuFrames {
    device: RenderDevice,
    queue: RenderQueue,
    pipeline: wgpu::ComputePipeline,
    slots: [Slot; RING_SIZE],
    next_slot: usize,
    profiling: bool,
    errors: Vec<GpuError>,
}
impl GpuFrames {
    #[cfg(test)]
    pub fn from_wgpu(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self::new(
            device.into(),
            RenderQueue(Arc::new(WgpuWrapper::new(queue))),
        )
    }
    pub fn new(device: RenderDevice, queue: RenderQueue) -> Self {
        Self::with_run_capacity(device, queue, DEFAULT_RUN_CAPACITY)
    }
    /// A smaller capacity is useful for deliberately exercising overflow.
    pub fn with_run_capacity(device: RenderDevice, queue: RenderQueue, capacity: u32) -> Self {
        let capacity = capacity
            .max(1)
            .checked_next_power_of_two()
            .expect("run capacity overflow");
        assert!(payload_bytes(capacity) <= device.limits().max_storage_buffer_binding_size);
        let raw = device.wgpu_device();
        let shader = raw.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("avatar exact alpha mask"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_mask.wgsl").into()),
        });
        let pipeline = raw.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("avatar exact alpha mask"),
            layout: None,
            module: &shader,
            entry_point: Some("mask"),
            compilation_options: Default::default(),
            cache: None,
        });
        let slots = std::array::from_fn(|_| Slot::new(raw, capacity));
        Self {
            device,
            queue,
            pipeline,
            slots,
            next_slot: 0,
            profiling: false,
            errors: Vec::new(),
        }
    }
    /// Opt-in only: device support alone never enables production timestamp
    /// readbacks. Existing submissions keep the setting captured at enqueue.
    pub fn set_profiling(&mut self, enabled: bool) {
        self.profiling = enabled;
    }
    /// Counts pending captures and completed frames still leased by the caller.
    #[allow(
        dead_code,
        reason = "integration diagnostics can report ring occupancy"
    )]
    pub fn occupied(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.pending.is_some() || Arc::strong_count(&s.lease) > 1)
            .count()
    }
    pub fn take_errors(&mut self) -> Vec<GpuError> {
        std::mem::take(&mut self.errors)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue(
        &mut self,
        source: &wgpu::Texture,
        width: u32,
        height: u32,
        scale: u32,
        generation: u64,
        sequence: u64,
        cpu_runs: Option<Vec<Run>>,
    ) -> bool {
        let start = Instant::now();
        if width == 0
            || height == 0
            || scale == 0
            || source.format() != wgpu::TextureFormat::Rgba8UnormSrgb
            || source.dimension() != wgpu::TextureDimension::D2
            || source.sample_count() != 1
            || source.width() != width
            || source.height() != height
            || source.depth_or_array_layers() != 1
            || !source.usage().contains(wgpu::TextureUsages::COPY_SRC)
            || width.div_ceil(scale) > i32::MAX as u32
            || height.div_ceil(scale) > i32::MAX as u32
            || height.div_ceil(scale) > self.device.limits().max_compute_workgroups_per_dimension
            || width.div_ceil(scale).div_ceil(MASK_PIXELS_PER_WORKGROUP)
                > self.device.limits().max_compute_workgroups_per_dimension
        {
            self.errors.push(GpuError(
                "invalid avatar GPU source/dimensions/scale or dispatch exceeds device limits"
                    .into(),
            ));
            return false;
        }
        let Some(index) = (0..RING_SIZE)
            .map(|n| (self.next_slot + n) % RING_SIZE)
            .find(|&i| {
                self.slots[i].pending.is_none() && Arc::strong_count(&self.slots[i].lease) == 1
            })
        else {
            return false;
        };
        self.next_slot = (index + 1) % RING_SIZE;
        let slot = &mut self.slots[index];
        if slot
            .texture
            .as_ref()
            .is_none_or(|t| t.width() != width || t.height() != height)
        {
            slot.texture = Some(self.device.wgpu_device().create_texture(
                &wgpu::TextureDescriptor {
                    label: Some("avatar immutable ring frame"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::COPY_DST
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            ));
        }
        slot.pending = Some(Pending {
            width,
            height,
            scale,
            generation,
            sequence,
            cpu_runs,
            receiver: None,
            timings: FrameTimings {
                enqueued_at: start,
                submitted_at: start,
                completed_at: start,
                decoded_at: start,
                cpu_submit: Duration::ZERO,
                cpu_decode: Duration::ZERO,
                gpu_copy: None,
                gpu_copy_mask: None,
                gpu_mask: None,
                mask_readback_bytes: 0,
                timestamp_readback_bytes: 0,
                overflow_retries: 0,
            },
            callbacks_remaining: 0,
            completion_error: None,
            timestamped: false,
            includes_copy: false,
        });
        submit_slot(
            self.device.wgpu_device(),
            &self.queue,
            &self.pipeline,
            slot,
            Some(source),
            self.profiling,
        );
        slot.pending.as_mut().unwrap().timings.cpu_submit += start.elapsed();
        true
    }

    /// Nonblocking. Map/device errors are available via `take_errors`; an
    /// incomplete or truncated mask is never returned as a completed frame.
    pub fn poll(&mut self) -> Vec<Frame> {
        if let Err(error) = self.device.wgpu_device().poll(wgpu::PollType::Poll) {
            self.errors
                .push(GpuError(format!("avatar device poll: {error}")));
        }
        let mut frames = Vec::with_capacity(RING_SIZE);
        for slot in &mut self.slots {
            let Some(mut pending) = slot.pending.take() else {
                continue;
            };
            while pending.callbacks_remaining > 0 {
                match pending.receiver.as_ref().unwrap().try_recv() {
                    Ok(completion) => {
                        pending.callbacks_remaining -= 1;
                        pending.timings.completed_at = completion.at;
                        if let Err(error) = completion.result {
                            pending.completion_error = Some(error);
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        pending.callbacks_remaining = 0;
                        pending.completion_error = Some("avatar GPU callback disconnected".into());
                    }
                }
            }
            if pending.callbacks_remaining > 0 {
                slot.pending = Some(pending);
                continue;
            }
            if let Some(error) = pending.completion_error.take() {
                slot.staging.unmap();
                if let Some(t) = &slot.timestamps {
                    t.staging.unmap();
                }
                self.errors.push(GpuError(error));
                continue;
            }
            let decode_start = Instant::now();
            if pending.timestamped {
                let t = slot.timestamps.as_ref().unwrap();
                let view = t.staging.slice(..).get_mapped_range();
                let tick =
                    |offset| u64::from_le_bytes(view[offset..offset + 8].try_into().unwrap());
                let duration = |first: u64, last: u64| {
                    Duration::from_secs_f64(
                        last.wrapping_sub(first) as f64
                            * f64::from(self.queue.get_timestamp_period())
                            / 1e9,
                    )
                };
                let (mask_start, mask_end) = if pending.includes_copy {
                    pending.timings.gpu_copy = Some(duration(tick(0), tick(8)));
                    if pending.cpu_runs.is_none() {
                        pending.timings.gpu_copy_mask = Some(duration(tick(0), tick(16)));
                        (tick(8), tick(16))
                    } else {
                        (tick(8), tick(8))
                    }
                } else {
                    (tick(0), tick(8))
                };
                if pending.cpu_runs.is_none() {
                    let elapsed = duration(mask_start, mask_end);
                    pending.timings.gpu_mask =
                        Some(pending.timings.gpu_mask.unwrap_or_default() + elapsed);
                    if !pending.includes_copy
                        && let Some(total) = &mut pending.timings.gpu_copy_mask
                    {
                        *total += elapsed;
                    }
                }
                drop(view);
                t.staging.unmap();
            }
            let mask_readback = pending.cpu_runs.is_none();
            let runs = if let Some(runs) = pending.cpu_runs.take() {
                runs
            } else {
                let view = slot.staging.slice(..).get_mapped_range();
                let count = read_u32(&view, 0);
                let overflow = read_u32(&view, 4);
                if overflow != 0 || count > slot.capacity {
                    drop(view);
                    slot.staging.unmap();
                    pending.timings.cpu_decode += decode_start.elapsed();
                    let capacity = count
                        .max(slot.capacity.saturating_add(1))
                        .checked_next_power_of_two();
                    let Some(capacity) = capacity.filter(|&capacity| {
                        payload_bytes(capacity)
                            <= self.device.limits().max_storage_buffer_binding_size
                            && payload_bytes(capacity) <= self.device.limits().max_buffer_size
                    }) else {
                        self.errors.push(GpuError(format!(
                            "exact avatar mask needs {count} runs; device storage limit exceeded"
                        )));
                        continue;
                    };
                    let retry_start = Instant::now();
                    slot.grow(self.device.wgpu_device(), capacity);
                    pending.timings.overflow_retries += 1;
                    let pending_profiling = pending.timestamped;
                    slot.pending = Some(pending);
                    submit_slot(
                        self.device.wgpu_device(),
                        &self.queue,
                        &self.pipeline,
                        slot,
                        None,
                        pending_profiling,
                    );
                    slot.pending.as_mut().unwrap().timings.cpu_submit += retry_start.elapsed();
                    continue;
                }
                let mut runs = Vec::with_capacity(count as usize);
                let mut valid = true;
                for index in 0..count as usize {
                    let offset = HEADER_BYTES as usize + index * RUN_BYTES as usize;
                    let x = read_u32(&view, offset);
                    let y = read_u32(&view, offset + 4);
                    let width = read_u32(&view, offset + 8);
                    valid &= read_u32(&view, offset + 12) == 1
                        && width > 0
                        && x < pending.width.div_ceil(pending.scale)
                        && width <= pending.width.div_ceil(pending.scale) - x
                        && y < pending.height.div_ceil(pending.scale);
                    runs.push(Run {
                        x: x as i32,
                        y: y as i32,
                        width: width as i32,
                    });
                }
                drop(view);
                slot.staging.unmap();
                if !valid {
                    self.errors
                        .push(GpuError("invalid run in avatar mask readback".into()));
                    continue;
                }
                runs.sort_unstable_by_key(|r| (r.y, r.x));
                runs
            };
            pending.timings.cpu_decode += decode_start.elapsed();
            pending.timings.decoded_at = Instant::now();
            frames.push(Frame {
                texture: slot.texture.as_ref().unwrap().clone(),
                width: pending.width,
                height: pending.height,
                scale: pending.scale,
                generation: pending.generation,
                sequence: pending.sequence,
                runs,
                mask_readback,
                timings: pending.timings,
                _lease: slot.lease.clone(),
            });
        }
        frames.sort_unstable_by_key(|f| f.sequence);
        frames
    }
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn submit_slot(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::ComputePipeline,
    slot: &mut Slot,
    source: Option<&wgpu::Texture>,
    profiling: bool,
) {
    let pending = slot.pending.as_mut().unwrap();
    let texture = slot.texture.as_ref().unwrap();
    let mask = pending.cpu_runs.is_none();
    let encoder_timestamps = device
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    pending.timestamped = profiling && slot.timestamps.is_some() && (mask || encoder_timestamps);
    pending.includes_copy = encoder_timestamps && source.is_some();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("avatar frame copy/mask"),
    });
    if pending.timestamped && pending.includes_copy {
        encoder.write_timestamp(&slot.timestamps.as_ref().unwrap().queries, 0);
    }
    if let Some(source) = source {
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            texture.as_image_copy(),
            wgpu::Extent3d {
                width: pending.width,
                height: pending.height,
                depth_or_array_layers: 1,
            },
        );
    }
    if mask {
        encoder.clear_buffer(&slot.storage, 0, Some(HEADER_BYTES));
        let params = [pending.width, pending.height, pending.scale, slot.capacity];
        let bytes: Vec<u8> = params.into_iter().flat_map(u32::to_le_bytes).collect();
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avatar mask parameters"),
            contents: &bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = texture.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("avatar mask bindings"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: slot.storage.as_entire_binding(),
                },
            ],
        });
        if pending.timestamped && encoder_timestamps {
            encoder.write_timestamp(&slot.timestamps.as_ref().unwrap().queries, 1);
        }
        {
            let timestamp_writes = if pending.timestamped && !encoder_timestamps {
                Some(wgpu::ComputePassTimestampWrites {
                    query_set: &slot.timestamps.as_ref().unwrap().queries,
                    beginning_of_pass_write_index: Some(1),
                    end_of_pass_write_index: Some(2),
                })
            } else {
                None
            };
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("avatar scan interval starts"),
                timestamp_writes,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(
                pending
                    .width
                    .div_ceil(pending.scale)
                    .div_ceil(MASK_PIXELS_PER_WORKGROUP),
                pending.height.div_ceil(pending.scale),
                1,
            );
        }
        if pending.timestamped && encoder_timestamps {
            encoder.write_timestamp(&slot.timestamps.as_ref().unwrap().queries, 2);
        }
        let bytes = payload_bytes(slot.capacity);
        encoder.copy_buffer_to_buffer(&slot.storage, 0, &slot.staging, 0, bytes);
        pending.timings.mask_readback_bytes += bytes;
    }
    if pending.timestamped {
        let t = slot.timestamps.as_ref().unwrap();
        if !mask {
            encoder.write_timestamp(&t.queries, 1);
        }
        let (range, bytes) = if pending.includes_copy && mask {
            (0..3, 24)
        } else if pending.includes_copy {
            (0..2, 16)
        } else {
            (1..3, 16)
        };
        encoder.resolve_query_set(&t.queries, range, &t.resolve, 0);
        encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.staging, 0, bytes);
        pending.timings.timestamp_readback_bytes += bytes;
    }
    pending.timings.submitted_at = Instant::now();
    queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::channel();
    pending.callbacks_remaining = 0;
    if pending.timestamped {
        let sender = sender.clone();
        pending.callbacks_remaining += 1;
        slot.timestamps
            .as_ref()
            .unwrap()
            .staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(Completion {
                    result: result.map_err(|error| error.to_string()),
                    at: Instant::now(),
                });
            });
    }
    if mask {
        pending.callbacks_remaining += 1;
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(Completion {
                    result: result.map_err(|error| error.to_string()),
                    at: Instant::now(),
                });
            });
    } else if !pending.timestamped {
        pending.callbacks_remaining += 1;
        queue.on_submitted_work_done(move || {
            let _ = sender.send(Completion {
                result: Ok(()),
                at: Instant::now(),
            });
        });
    }
    pending.receiver = Some(receiver);
}

impl Frame {
    /// Explicit verification only. Production capture never copies image or
    /// alpha pixels to the CPU. This readback holds a frame lease until dropped.
    pub fn readback_rgba(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> VerificationReadback {
        let row_bytes = self.width * 4;
        let stride = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("avatar optional verification pixels"),
            size: u64::from(stride) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("avatar optional verification copy"),
        });
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result.map_err(|error| error.to_string()));
            });
        VerificationReadback {
            buffer,
            receiver,
            frame: self.clone(),
            stride,
            finished: false,
        }
    }
}
pub struct VerificationReadback {
    buffer: wgpu::Buffer,
    receiver: mpsc::Receiver<Result<(), String>>,
    frame: Frame,
    stride: u32,
    finished: bool,
}
impl VerificationReadback {
    /// Returns tightly packed, unmodified RGBA target bytes, once available.
    pub fn poll(&mut self, device: &wgpu::Device) -> Result<Option<Vec<u8>>, GpuError> {
        if self.finished {
            return Ok(None);
        }
        device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| GpuError(error.to_string()))?;
        match self.receiver.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(GpuError("verification callback disconnected".into()));
            }
            Ok(Err(error)) => {
                self.finished = true;
                return Err(GpuError(error));
            }
            Ok(Ok(())) => {}
        }
        let view = self.buffer.slice(..).get_mapped_range();
        let mut rgba =
            Vec::with_capacity(self.frame.width as usize * self.frame.height as usize * 4);
        for row in view.chunks_exact(self.stride as usize) {
            rgba.extend_from_slice(&row[..self.frame.width as usize * 4]);
        }
        drop(view);
        self.buffer.unmap();
        self.finished = true;
        Ok(Some(rgba))
    }
}

/// Presenter sharing Bevy's instance/adapter/device/queue. Creation retains
/// owned window handles so a genuinely lost surface can be recreated safely.
pub struct GpuSurface {
    instance: wgpu::Instance,
    window: Arc<dyn WindowHandles>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    needs_reconfigure: bool,
    profiling: bool,
    pass_timings: PassTimings,
}
trait WindowHandles: HasWindowHandle + HasDisplayHandle + Send + Sync {}
impl<T: HasWindowHandle + HasDisplayHandle + Send + Sync> WindowHandles for T {}
impl GpuSurface {
    pub fn new<W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static>(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        window: Arc<W>,
    ) -> Result<Self, GpuError> {
        let window: Arc<dyn WindowHandles> = window;
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| GpuError(error.to_string()))?;
        let capabilities = surface.get_capabilities(adapter);
        let format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .ok_or_else(|| GpuError("avatar surface requires BGRA8/RGBA8 plain UNORM".into()))?;
        let alpha_mode = choose_alpha_mode(&capabilities.alpha_modes)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: 1,
            height: 1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        let pipeline = presentation_pipeline(device, format, alpha_mode);
        Ok(Self {
            instance: instance.clone(),
            window,
            surface,
            device: device.clone(),
            queue: queue.clone(),
            config,
            pipeline,
            needs_reconfigure: true,
            profiling: false,
            pass_timings: PassTimings::new(),
        })
    }
    /// Timestamp only the conversion render pass; acquisition/CPU/queue idle
    /// outside the pass are excluded. Disabled by default, even if supported.
    pub fn set_profiling(&mut self, enabled: bool) {
        self.profiling = enabled;
    }
    /// Missing samples mean disabled/unsupported timestamps or timing-ring
    /// saturation, never a fabricated zero-duration GPU measurement.
    pub fn poll_timings(&mut self) -> Vec<(u64, Duration)> {
        self.pass_timings.poll(&self.device, &self.queue)
    }
    pub fn take_errors(&mut self) -> Vec<GpuError> {
        std::mem::take(&mut self.pass_timings.errors)
    }
    #[allow(
        dead_code,
        reason = "native diagnostics can report the negotiated surface format"
    )]
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }
    #[allow(
        dead_code,
        reason = "native diagnostics can report the negotiated alpha mode"
    )]
    pub fn alpha_mode(&self) -> wgpu::CompositeAlphaMode {
        self.config.alpha_mode
    }

    /// Width/height must match the physical source extent. The command is
    /// submitted before returning; the caller installs input coverage and
    /// double-buffered Wayland state, then calls `SurfaceTexture::present`.
    /// Acquisition failures skip this frame; reconfiguration occurs next call.
    pub fn draw(
        &mut self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> Result<Option<wgpu::SurfaceTexture>, GpuError> {
        self.draw_inner(texture, width, height, None)
    }
    /// Same presentation behavior as `draw`, plus sequence-associated optional
    /// render-pass timestamps. Call `set_profiling(true)` and `poll_timings`.
    pub fn draw_timed(
        &mut self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        sequence: u64,
    ) -> Result<Option<wgpu::SurfaceTexture>, GpuError> {
        if !self.profiling {
            return self.draw(texture, width, height);
        }
        self.draw_inner(texture, width, height, Some(sequence))
    }
    fn draw_inner(
        &mut self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        sequence: Option<u64>,
    ) -> Result<Option<wgpu::SurfaceTexture>, GpuError> {
        if width == 0 || height == 0 {
            return Ok(None);
        }
        if texture.width() != width
            || texture.height() != height
            || texture.format() != wgpu::TextureFormat::Rgba8UnormSrgb
            || texture.sample_count() != 1
            || texture.dimension() != wgpu::TextureDimension::D2
            || texture.depth_or_array_layers() != 1
            || !texture
                .usage()
                .contains(wgpu::TextureUsages::TEXTURE_BINDING)
        {
            return Err(GpuError(
                "avatar presenter requires a matching sampled RGBA8 SRGB frame".into(),
            ));
        }
        if self.needs_reconfigure || self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
            self.needs_reconfigure = false;
        }
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => output,
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => {
                self.needs_reconfigure = true;
                output
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.needs_reconfigure = true;
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self
                    .instance
                    .create_surface(self.window.clone())
                    .map_err(|error| GpuError(error.to_string()))?;
                self.needs_reconfigure = true;
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(GpuError(
                    "avatar surface acquisition validation failure".into(),
                ));
            }
        };
        let target = output.texture.create_view(&Default::default());
        let timing = sequence
            .filter(|_| self.profiling)
            .and_then(|sequence| self.pass_timings.reserve(&self.device, sequence));
        draw_texture_profiled(
            &self.device,
            &self.queue,
            &self.pipeline,
            texture,
            &target,
            timing,
        );
        Ok(Some(output))
    }
}
fn choose_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
) -> Result<wgpu::CompositeAlphaMode, GpuError> {
    [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::PostMultiplied,
    ]
    .into_iter()
    .find(|mode| modes.contains(mode))
    .ok_or_else(|| {
        GpuError(
            "avatar surface does not support transparent premultiplied/postmultiplied compositing"
                .into(),
        )
    })
}
fn presentation_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("avatar SRGB premultiplication conversion"),
        source: wgpu::ShaderSource::Wgsl(include_str!("gpu_present.wgsl").into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("avatar transparent fullscreen presentation"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("fullscreen"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some(if alpha_mode == wgpu::CompositeAlphaMode::PostMultiplied {
                "postmultiplied"
            } else {
                "premultiplied"
            }),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}
#[cfg(test)]
fn draw_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    texture: &wgpu::Texture,
    target: &wgpu::TextureView,
) {
    draw_texture_profiled(device, queue, pipeline, texture, target, None);
}
fn draw_texture_profiled(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    texture: &wgpu::Texture,
    target: &wgpu::TextureView,
    timing: Option<&mut PassTiming>,
) {
    let source = texture.create_view(&Default::default());
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("avatar presentation source"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&source),
        }],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("avatar presentation"),
    });
    {
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("avatar fullscreen conversion"),
            color_attachments: &attachments,
            timestamp_writes: timing
                .as_ref()
                .map(|timing| wgpu::RenderPassTimestampWrites {
                    query_set: &timing.queries,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }
    if let Some(timing) = timing.as_ref() {
        encoder.resolve_query_set(&timing.queries, 0..2, &timing.resolve, 0);
        encoder.copy_buffer_to_buffer(&timing.resolve, 0, &timing.staging, 0, 16);
    }
    queue.submit([encoder.finish()]);
    if let Some(timing) = timing {
        let (sender, receiver) = mpsc::channel();
        timing
            .staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result.map_err(|error| error.to_string()));
            });
        timing.receiver = Some(receiver);
    }
}

struct PassTiming {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    staging: wgpu::Buffer,
    sequence: u64,
    receiver: Option<mpsc::Receiver<Result<(), String>>>,
}
struct PassTimings {
    slots: [Option<PassTiming>; RING_SIZE],
    errors: Vec<GpuError>,
}
impl PassTimings {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            errors: vec![],
        }
    }
    fn reserve(&mut self, device: &wgpu::Device, sequence: u64) -> Option<&mut PassTiming> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_none_or(|s| s.receiver.is_none()))?;
        let slot = slot.get_or_insert_with(|| PassTiming {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("avatar presentation pass timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("avatar presentation timestamp resolve"),
                size: 16,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            staging: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("avatar presentation timestamp staging"),
                size: 16,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            sequence,
            receiver: None,
        });
        slot.sequence = sequence;
        Some(slot)
    }
    fn poll(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<(u64, Duration)> {
        if self
            .slots
            .iter()
            .all(|slot| slot.as_ref().is_none_or(|s| s.receiver.is_none()))
        {
            return vec![];
        }
        if let Err(error) = device.poll(wgpu::PollType::Poll) {
            self.errors.push(GpuError(format!(
                "presentation timestamp device poll: {error}"
            )));
        }
        let mut samples = Vec::with_capacity(RING_SIZE);
        for slot in self.slots.iter_mut().flatten() {
            let Some(receiver) = &slot.receiver else {
                continue;
            };
            match receiver.try_recv() {
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.errors.push(GpuError(
                        "presentation timestamp callback disconnected".into(),
                    ));
                }
                Ok(Err(error)) => self.errors.push(GpuError(error)),
                Ok(Ok(())) => {
                    let view = slot.staging.slice(..).get_mapped_range();
                    let first = u64::from_le_bytes(view[0..8].try_into().unwrap());
                    let last = u64::from_le_bytes(view[8..16].try_into().unwrap());
                    if let Some(duration) =
                        timestamp_duration(first, last, queue.get_timestamp_period())
                    {
                        samples.push((slot.sequence, duration));
                    } else {
                        self.errors
                            .push(GpuError("invalid presentation pass timestamp span".into()));
                    }
                    drop(view);
                }
            }
            slot.staging.unmap();
            slot.receiver = None;
        }
        samples.sort_unstable_by_key(|(sequence, _)| *sequence);
        samples
    }
}
fn timestamp_duration(first: u64, last: u64, period: f32) -> Option<Duration> {
    let ticks = last.checked_sub(first)?;
    (period.is_finite() && period > 0.0)
        .then(|| Duration::from_secs_f64(ticks as f64 * f64::from(period) / 1e9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        task::{Context, Poll, Wake, Waker},
    };

    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park_timeout(Duration::from_millis(10)),
            }
        }
    }
    fn device(timestamped: bool) -> (wgpu::Device, wgpu::Queue) {
        device_with_features(if timestamped {
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
        } else {
            wgpu::Features::empty()
        })
    }
    fn device_with_features(requested: wgpu::Features) -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        }))
        .expect("synthetic GPU tests require a Vulkan software adapter (e.g. lavapipe)");
        let features = adapter.features() & requested;
        block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("avatar synthetic GPU tests"),
            required_features: features,
            ..Default::default()
        }))
        .expect("test GPU device")
    }
    fn texture(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("avatar synthetic frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }
    fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, rgba: &[u8]) {
        queue.write_texture(
            texture.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(texture.width() * 4),
                rows_per_image: Some(texture.height()),
            },
            texture.size(),
        );
        queue.submit([]);
    }
    fn complete(frames: &mut GpuFrames, count: usize) -> Vec<Frame> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut result = Vec::new();
        while result.len() < count {
            result.extend(frames.poll());
            assert!(
                frames.take_errors().is_empty(),
                "GPU ring reported an error"
            );
            assert!(Instant::now() < deadline, "GPU map timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        result.sort_unstable_by_key(|frame| frame.sequence);
        result
    }
    fn verify(frame: &Frame, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut readback = frame.readback_rgba(device, queue);
        loop {
            if let Some(rgba) = readback.poll(device).unwrap() {
                return rgba;
            }
            assert!(Instant::now() < deadline, "verification map timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Vec<u8> {
        let now = Instant::now();
        let frame = Frame {
            texture: texture.clone(),
            width: texture.width(),
            height: texture.height(),
            scale: 1,
            generation: 0,
            sequence: 0,
            runs: vec![],
            mask_readback: false,
            timings: FrameTimings {
                enqueued_at: now,
                submitted_at: now,
                completed_at: now,
                decoded_at: now,
                cpu_submit: Duration::ZERO,
                cpu_decode: Duration::ZERO,
                gpu_copy: None,
                gpu_copy_mask: None,
                gpu_mask: None,
                mask_readback_bytes: 0,
                timestamp_readback_bytes: 0,
                overflow_retries: 0,
            },
            _lease: Arc::new(()),
        };
        verify(&frame, device, queue)
    }

    #[test]
    fn mask_rows_cross_workgroup_boundaries_without_dropping_the_tail() {
        let (device, queue) = device(false);
        let (width, height) = (9, 131);
        let rgba: Vec<_> = (0..width * height)
            .flat_map(|n| [255, 30, 10, [0, 8, 9, 128, 255][n as usize % 5]])
            .collect();
        let source = texture(&device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb);
        upload(&queue, &source, &rgba);
        let mut frames = GpuFrames::from_wgpu(device, queue);
        for scale in [1, 2] {
            assert!(frames.enqueue(&source, width, height, scale, 1, u64::from(scale), None));
            let frame = complete(&mut frames, 1).pop().unwrap();
            assert_eq!(
                frame.runs,
                crate::mask::alpha_runs(&rgba, width, height, scale)
            );
        }
    }

    #[test]
    fn exact_mask_all_alpha_bytes_partial_blocks_and_metadata() {
        let (device, queue) = device(false);
        let width = 257;
        let height = 7;
        let mut rgba = vec![0; width as usize * height as usize * 4];
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                let alpha = if y == 0 {
                    x.min(255) as u8
                } else {
                    [0, 8, 9, 255, 1, 7, 128][((x * 11 + y * 17) % 7) as usize]
                };
                rgba[offset..offset + 4].copy_from_slice(&[200, 17, 255, alpha]);
            }
        }
        let source = texture(&device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb);
        upload(&queue, &source, &rgba);
        let mut frames = GpuFrames::from_wgpu(device.clone(), queue.clone());
        for scale in [1, 2, 3, 4, 512] {
            assert!(frames.enqueue(&source, width, height, scale, 73, u64::from(scale), None));
            let frame = complete(&mut frames, 1).pop().unwrap();
            assert_eq!(
                frame.runs,
                crate::mask::alpha_runs(&rgba, width, height, scale)
            );
            assert_eq!(
                (
                    frame.width,
                    frame.height,
                    frame.scale,
                    frame.generation,
                    frame.sequence
                ),
                (width, height, scale, 73, u64::from(scale))
            );
            assert!(frame.mask_readback);
            assert_eq!(
                frame.timings.mask_readback_bytes,
                payload_bytes(DEFAULT_RUN_CAPACITY)
            );
            assert_eq!(frame.timings.timestamp_readback_bytes, 0);
            assert!(frame.timings.enqueued_at <= frame.timings.submitted_at);
            assert!(frame.timings.submitted_at <= frame.timings.completed_at);
            assert!(frame.timings.completed_at <= frame.timings.decoded_at);
            assert_eq!(verify(&frame, &device, &queue), rgba);
        }
        let mut transparent = rgba.clone();
        for pixel in transparent.as_chunks_mut::<4>().0 {
            pixel[3] = 8;
        }
        upload(&queue, &source, &transparent);
        assert!(frames.enqueue(&source, width, height, 1, 74, 513, None));
        assert!(
            complete(&mut frames, 1)[0].runs.is_empty(),
            "alpha byte 8 is never opaque"
        );
    }

    #[test]
    fn overflow_retries_original_texture_losslessly_with_async_timestamps() {
        let (device, queue) = device(true);
        let (width, height) = (257, 97);
        let mut rgba = vec![0; width as usize * height as usize * 4];
        for y in 0..height {
            for x in 0..width {
                rgba[((y * width + x) * 4 + 3) as usize] = if x % 2 == 0 { 9 } else { 8 };
            }
        }
        let source = texture(&device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb);
        upload(&queue, &source, &rgba);
        let mut frames = GpuFrames::from_wgpu(device.clone(), queue.clone());
        frames.set_profiling(true);
        assert!(frames.enqueue(&source, width, height, 1, 9, 19, None));
        // Change the live render target before any callback or overflow retry.
        upload(&queue, &source, &vec![0; rgba.len()]);
        let frame = complete(&mut frames, 1).pop().unwrap();
        let expected = crate::mask::alpha_runs(&rgba, width, height, 1);
        assert!(expected.len() > DEFAULT_RUN_CAPACITY as usize);
        assert_eq!(frame.runs, expected);
        assert_eq!(frame.timings.overflow_retries, 1);
        assert_eq!(
            frame.timings.mask_readback_bytes,
            payload_bytes(DEFAULT_RUN_CAPACITY)
                + payload_bytes((expected.len() as u32).next_power_of_two())
        );
        assert_eq!(verify(&frame, &device, &queue), rgba);
        if device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            let expected_timestamp_bytes = if device
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
            {
                24 + 16
            } else {
                16 + 16
            };
            assert_eq!(
                frame.timings.timestamp_readback_bytes,
                expected_timestamp_bytes
            );
            assert!(frame.timings.gpu_mask.is_some());
            if device
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
            {
                assert!(frame.timings.gpu_copy_mask.is_some());
            }
        }
    }

    #[test]
    fn three_slot_backpressure_cpu_coverage_and_recycle_after_draw() {
        let (device, queue) = device(false);
        let (width, height) = (3, 5);
        let source = texture(&device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb);
        let rgba = vec![255; width as usize * height as usize * 4];
        upload(&queue, &source, &rgba);
        let supplied = vec![Run {
            x: 0,
            y: 1,
            width: 2,
        }];
        let mut frames = GpuFrames::from_wgpu(device.clone(), queue.clone());
        for sequence in 1..=3 {
            assert!(frames.enqueue(
                &source,
                width,
                height,
                1,
                11,
                sequence,
                Some(if sequence == 2 {
                    supplied.clone()
                } else {
                    vec![]
                })
            ));
        }
        assert_eq!(frames.occupied(), 3);
        assert!(!frames.enqueue(&source, width, height, 1, 11, 4, None));
        let mut completed = complete(&mut frames, 3);
        for frame in &completed {
            assert!(!frame.mask_readback);
            assert_eq!(frame.timings.mask_readback_bytes, 0);
            assert_eq!(frame.timings.timestamp_readback_bytes, 0);
            assert_eq!(verify(frame, &device, &queue), rgba);
        }
        assert_eq!(completed[1].runs, supplied);
        assert!(completed[0].runs.is_empty());
        assert_eq!(
            frames.occupied(),
            3,
            "published frame leases still reserve slots"
        );
        let frame = completed.pop().unwrap();
        let duplicate = frame.clone();
        drop(frame);
        assert!(!frames.enqueue(&source, width, height, 1, 11, 4, None));
        let output = texture(&device, width, height, wgpu::TextureFormat::Rgba8Unorm);
        let pipeline = presentation_pipeline(
            &device,
            output.format(),
            wgpu::CompositeAlphaMode::PreMultiplied,
        );
        draw_texture(
            &device,
            &queue,
            &pipeline,
            &duplicate.texture,
            &output.create_view(&Default::default()),
        );
        drop(duplicate);
        // Copy a new scene into the recycled slot; submission ordering must
        // leave the preceding draw reading the previous immutable snapshot.
        upload(&queue, &source, &vec![0; rgba.len()]);
        assert!(frames.enqueue(&source, width, height, 1, 12, 4, Some(vec![])));
        assert_eq!(read_texture(&device, &queue, &output), rgba);
        let replacement = complete(&mut frames, 1).pop().unwrap();
        assert_eq!(replacement.generation, 12);
        assert_eq!(verify(&replacement, &device, &queue), vec![0; rgba.len()]);
    }

    #[test]
    fn timestamp_readbacks_are_opt_in_and_pass_only_mask_is_measured() {
        let (device, queue) = device_with_features(wgpu::Features::TIMESTAMP_QUERY);
        let source = texture(&device, 3, 5, wgpu::TextureFormat::Rgba8UnormSrgb);
        let rgba = vec![255; 3 * 5 * 4];
        upload(&queue, &source, &rgba);
        let mut frames = GpuFrames::from_wgpu(device.clone(), queue.clone());
        assert!(frames.enqueue(&source, 3, 5, 1, 1, 1, None));
        let frame = complete(&mut frames, 1).pop().unwrap();
        assert!(frame.timings.gpu_mask.is_none());
        assert_eq!(frame.timings.timestamp_readback_bytes, 0);
        drop(frame);
        frames.set_profiling(true);
        assert!(frames.enqueue(&source, 3, 5, 1, 1, 2, None));
        let frame = complete(&mut frames, 1).pop().unwrap();
        assert_eq!(frame.runs, crate::mask::alpha_runs(&rgba, 3, 5, 1));
        if device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            assert!(frame.timings.gpu_mask.is_some());
            assert_eq!(frame.timings.timestamp_readback_bytes, 16);
        }
        assert!(frame.timings.gpu_copy.is_none());
        assert!(frame.timings.gpu_copy_mask.is_none());
        drop(frame);
        assert!(frames.enqueue(&source, 3, 5, 1, 1, 3, Some(vec![])));
        let frame = complete(&mut frames, 1).pop().unwrap();
        assert!(!frame.mask_readback);
        assert_eq!(frame.timings.mask_readback_bytes, 0);
        assert_eq!(frame.timings.timestamp_readback_bytes, 0);
    }

    #[test]
    fn presentation_pass_timing_is_async_bounded_and_sequence_associated() {
        let (device, queue) = device_with_features(wgpu::Features::TIMESTAMP_QUERY);
        let source = texture(&device, 3, 5, wgpu::TextureFormat::Rgba8UnormSrgb);
        upload(&queue, &source, &[255; 3 * 5 * 4]);
        let output = texture(&device, 3, 5, wgpu::TextureFormat::Rgba8Unorm);
        let pipeline = presentation_pipeline(
            &device,
            output.format(),
            wgpu::CompositeAlphaMode::PreMultiplied,
        );
        let target = output.create_view(&Default::default());
        let mut timings = PassTimings::new();
        // Untimed rendering never allocates/maps query buffers.
        draw_texture_profiled(&device, &queue, &pipeline, &source, &target, None);
        assert!(timings.poll(&device, &queue).is_empty());
        assert!(timings.slots.iter().all(Option::is_none));
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            assert!(timings.reserve(&device, 1).is_none());
            return;
        }
        for sequence in 7..=9 {
            let timing = timings
                .reserve(&device, sequence)
                .expect("free presentation timestamp slot");
            draw_texture_profiled(&device, &queue, &pipeline, &source, &target, Some(timing));
        }
        assert!(
            timings.reserve(&device, 10).is_none(),
            "timestamp ring is bounded at three"
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut samples = Vec::new();
        while samples.len() < 3 {
            samples.extend(timings.poll(&device, &queue));
            assert!(timings.errors.is_empty(), "{:?}", timings.errors);
            assert!(
                Instant::now() < deadline,
                "presentation timestamp map timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        samples.sort_unstable_by_key(|(sequence, _)| *sequence);
        assert_eq!(
            samples
                .iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            vec![7, 8, 9]
        );
        assert!(
            timings.poll(&device, &queue).is_empty(),
            "samples are delivered once"
        );
        let timing = timings
            .reserve(&device, 10)
            .expect("slot recycled after query decode");
        draw_texture_profiled(&device, &queue, &pipeline, &source, &target, Some(timing));
        loop {
            let samples = timings.poll(&device, &queue);
            assert!(timings.errors.is_empty(), "{:?}", timings.errors);
            if !samples.is_empty() {
                assert_eq!(samples[0].0, 10);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(read_texture(&device, &queue, &output), vec![255; 3 * 5 * 4]);
        assert_eq!(timestamp_duration(9, 8, 1.0), None);
        assert_eq!(timestamp_duration(1, 2, f32::NAN), None);
        assert_eq!(timestamp_duration(1, 2, 0.0), None);
    }

    #[test]
    fn invalid_sources_report_errors_and_do_not_occupy_ring_slots() {
        let (device, queue) = device(false);
        let source = texture(&device, 3, 5, wgpu::TextureFormat::Rgba8UnormSrgb);
        let plain = texture(&device, 3, 5, wgpu::TextureFormat::Rgba8Unorm);
        let mut frames = GpuFrames::from_wgpu(device.clone(), queue.clone());
        assert!(!frames.enqueue(&source, 0, 5, 1, 1, 1, None));
        assert!(!frames.enqueue(&source, 3, 5, 0, 1, 1, None));
        assert!(!frames.enqueue(&source, 4, 5, 1, 1, 1, None));
        assert!(!frames.enqueue(&plain, 3, 5, 1, 1, 1, None));
        assert_eq!(frames.occupied(), 0);
        assert_eq!(frames.take_errors().len(), 4);
        assert!(frames.take_errors().is_empty());
        assert!(frames.poll().is_empty());
    }

    #[test]
    fn presentation_matches_argb_all_alpha_bytes_and_surface_modes() {
        let (device, queue) = device(false);
        // Exhaust every color byte at every alpha, including alpha-zero RGB
        // garbage, clamped straight colors, and translucent white/hair values.
        let (width, height) = (256, 256);
        let mut rgba = Vec::with_capacity(width * height * 4);
        for alpha in 0..=255u8 {
            for color in 0..=255u8 {
                rgba.extend_from_slice(&[color, color.wrapping_mul(17), 255 - color, alpha]);
            }
        }
        let source = texture(
            &device,
            width as u32,
            height as u32,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        upload(&queue, &source, &rgba);
        for format in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8Unorm,
        ] {
            let output = texture(&device, width as u32, height as u32, format);
            let pipeline =
                presentation_pipeline(&device, format, wgpu::CompositeAlphaMode::PreMultiplied);
            draw_texture(
                &device,
                &queue,
                &pipeline,
                &source,
                &output.create_view(&Default::default()),
            );
            let bytes = read_texture(&device, &queue, &output);
            for (original, actual) in rgba.as_chunks::<4>().0.iter().zip(bytes.as_chunks::<4>().0) {
                let mut expected = crate::pixels::argb(original);
                if format == wgpu::TextureFormat::Rgba8Unorm {
                    expected.swap(0, 2);
                }
                assert_eq!(actual[3], expected[3], "alpha must remain byte exact");
                for channel in 0..3 {
                    assert!(
                        actual[channel].abs_diff(expected[channel]) <= 1,
                        "{format:?} input {original:?} output {actual:?} expected {expected:?}"
                    );
                }
                if original[3] == 0 {
                    assert_eq!(actual, &[0; 4]);
                }
            }
        }
        // PostMultiplied stores straight SRGB, leaving the compositor to apply
        // alpha. The visible result still agrees with the premultiplied oracle.
        let output = texture(
            &device,
            width as u32,
            height as u32,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let pipeline = presentation_pipeline(
            &device,
            output.format(),
            wgpu::CompositeAlphaMode::PostMultiplied,
        );
        draw_texture(
            &device,
            &queue,
            &pipeline,
            &source,
            &output.create_view(&Default::default()),
        );
        let bytes = read_texture(&device, &queue, &output);
        for (original, actual) in rgba.as_chunks::<4>().0.iter().zip(bytes.as_chunks::<4>().0) {
            let mut expected = crate::pixels::argb(original);
            expected.swap(0, 2);
            assert_eq!(actual[3], original[3]);
            for channel in 0..3 {
                let composited =
                    (f32::from(actual[channel]) * f32::from(actual[3]) / 255.0).round() as u8;
                assert!(composited.abs_diff(expected[channel]) <= 1);
            }
        }
        assert_eq!(
            choose_alpha_mode(&[
                wgpu::CompositeAlphaMode::PostMultiplied,
                wgpu::CompositeAlphaMode::PreMultiplied
            ])
            .unwrap(),
            wgpu::CompositeAlphaMode::PreMultiplied
        );
        assert_eq!(
            choose_alpha_mode(&[wgpu::CompositeAlphaMode::PostMultiplied]).unwrap(),
            wgpu::CompositeAlphaMode::PostMultiplied
        );
        assert!(choose_alpha_mode(&[wgpu::CompositeAlphaMode::Opaque]).is_err());
    }
}
