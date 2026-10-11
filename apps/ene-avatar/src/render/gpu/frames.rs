//! GPU-only frame transport on Bevy's existing device and queue.
//!
//! Call `enqueue` on the main thread after `renderer.app.update()`, using the
//! final `GpuImage` from `RenderApp`. Width/height are physical pixels; runs
//! are logical pixels. The final image always supplies the exact GPU mask.
//! Poll each tick and transfer complete frames to AvatarPresenter.
//!
//! AvatarPresenter retains the displayed LeasedFrame and prevents ring reuse. Three occupied or leased slots make `enqueue` return false; callers
//! should skip that capture and poll again, never block the render loop.
use crate::render::mask::Run;
use bevy::render::renderer::{RenderDevice, RenderQueue};
use std::sync::{Arc, mpsc};

use super::GpuError;
use wgpu::util::DeviceExt;
pub const RING_SIZE: usize = 3;
// Keep in sync with gpu_mask.wgsl. Adjacent pixels share SIMD lanes.
const MASK_PIXELS_PER_WORKGROUP: u32 = 256;
pub const DEFAULT_RUN_CAPACITY: u32 = 4096;
const HEADER_BYTES: u64 = 16;
const RUN_BYTES: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameIdentity {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub generation: u64,
    pub sequence: u64,
}

/// Immutable image, exact mask and ring lease cannot be separated by callers.
#[derive(Debug)]
pub struct LeasedFrame {
    pub(super) texture: wgpu::Texture,
    pub(super) identity: FrameIdentity,
    pub(super) runs: Vec<Run>,
    _lease: Arc<()>,
}
struct Pending {
    identity: FrameIdentity,
    receiver: mpsc::Receiver<Result<(), String>>,
}
struct Slot {
    texture: Option<wgpu::Texture>,
    storage: wgpu::Buffer,
    staging: wgpu::Buffer,
    capacity: u32,
    pending: Option<Pending>,
    lease: Arc<()>,
}
impl Slot {
    fn new(device: &wgpu::Device, capacity: u32) -> Self {
        let bytes = payload_bytes(capacity);
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
    errors: Vec<GpuError>,
}
impl GpuFrames {
    pub fn new(device: RenderDevice, queue: RenderQueue) -> Self {
        let capacity = DEFAULT_RUN_CAPACITY;
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
            errors: Vec::new(),
        }
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
    ) -> bool {
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
        slot.pending = Some(submit_slot(
            self.device.wgpu_device(),
            &self.queue,
            &self.pipeline,
            slot,
            FrameIdentity {
                width,
                height,
                scale,
                generation,
                sequence,
            },
            Some(source),
        ));
        true
    }

    /// Nonblocking. Map/device errors are available via `take_errors`; an
    /// incomplete or truncated mask is never returned as a completed frame.
    pub fn poll(&mut self) -> Vec<LeasedFrame> {
        if let Err(error) = self.device.wgpu_device().poll(wgpu::PollType::Poll) {
            self.errors
                .push(GpuError(format!("avatar device poll: {error}")));
        }
        let mut frames = Vec::with_capacity(RING_SIZE);
        for slot in &mut self.slots {
            let Some(pending) = slot.pending.take() else {
                continue;
            };
            match pending.receiver.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    slot.pending = Some(pending);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    slot.staging.unmap();
                    self.errors
                        .push(GpuError("avatar GPU callback disconnected".into()));
                    continue;
                }
                Ok(Err(error)) => {
                    slot.staging.unmap();
                    self.errors.push(GpuError(error));
                    continue;
                }
                Ok(Ok(())) => {}
            }
            let runs = {
                let view = slot.staging.slice(..).get_mapped_range();
                let count = read_u32(&view, 0);
                let overflow = read_u32(&view, 4);
                if overflow != 0 || count > slot.capacity {
                    drop(view);
                    slot.staging.unmap();
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
                    slot.grow(self.device.wgpu_device(), capacity);
                    slot.pending = Some(submit_slot(
                        self.device.wgpu_device(),
                        &self.queue,
                        &self.pipeline,
                        slot,
                        pending.identity,
                        None,
                    ));
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
                        && x < pending.identity.width.div_ceil(pending.identity.scale)
                        && width <= pending.identity.width.div_ceil(pending.identity.scale) - x
                        && y < pending.identity.height.div_ceil(pending.identity.scale);
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
            frames.push(LeasedFrame {
                texture: slot.texture.as_ref().unwrap().clone(),
                identity: pending.identity,
                runs,
                _lease: slot.lease.clone(),
            });
        }
        frames.sort_unstable_by_key(|f| f.identity.sequence);
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
    identity: FrameIdentity,
    source: Option<&wgpu::Texture>,
) -> Pending {
    let texture = slot.texture.as_ref().unwrap();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("avatar frame copy/mask"),
    });
    if let Some(source) = source {
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            texture.as_image_copy(),
            wgpu::Extent3d {
                width: identity.width,
                height: identity.height,
                depth_or_array_layers: 1,
            },
        );
    }
    {
        encoder.clear_buffer(&slot.storage, 0, Some(HEADER_BYTES));
        let params = [
            identity.width,
            identity.height,
            identity.scale,
            slot.capacity,
        ];
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
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("avatar scan interval starts"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(
                identity
                    .width
                    .div_ceil(identity.scale)
                    .div_ceil(MASK_PIXELS_PER_WORKGROUP),
                identity.height.div_ceil(identity.scale),
                1,
            );
        }
        let bytes = payload_bytes(slot.capacity);
        encoder.copy_buffer_to_buffer(&slot.storage, 0, &slot.staging, 0, bytes);
    }
    queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::channel();
    slot.staging
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result.map_err(|error| error.to_string()));
        });
    Pending { identity, receiver }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires a Vulkan adapter; run explicitly in the native verification environment"]
    async fn gpu_overflow_preserves_captured_masks_and_leases_prevent_ring_reuse() {
        use bevy::render::renderer::WgpuWrapper;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                force_fallback_adapter: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();
        let render_device = RenderDevice::from(device.clone());
        let render_queue = RenderQueue(Arc::new(WgpuWrapper::new(queue.clone())));
        let mut frames = GpuFrames::new(render_device, render_queue);
        let (width, height) = (512, 128);
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lease test source"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let mut bytes = vec![0; (width * height * 4) as usize];
        for y in 0..height {
            for x in (0..width).step_by(2) {
                let pixel = ((y * width + x) * 4) as usize;
                bytes[pixel..pixel + 4].fill(255);
            }
        }
        let upload = |bytes: &[u8]| {
            queue.write_texture(
                source.as_image_copy(),
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                source.size(),
            )
        };
        upload(&bytes);
        for sequence in 1..=3 {
            assert!(frames.enqueue(&source, width, height, 1, 7, sequence));
        }
        assert!(!frames.enqueue(&source, width, height, 1, 7, 4));
        // All overflow retries must still use the captured checkerboard, even
        // though the live render source now contains a different image.
        bytes.fill(0);
        upload(&bytes);
        let mut completed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while completed.len() < RING_SIZE {
            completed.extend(frames.poll());
            assert!(frames.take_errors().is_empty());
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        for frame in &completed {
            assert_eq!(frame.runs.len(), (width / 2 * height) as usize);
            assert_eq!(frame.identity.generation, 7);
            for run in &frame.runs {
                assert_eq!(run.width, 1);
                assert_eq!(run.x % 2, 0);
            }
        }
        assert!(
            !frames.enqueue(&source, width, height, 1, 7, 4),
            "leased textures must remain immutable"
        );
        completed.pop();
        assert!(
            frames.enqueue(&source, width, height, 1, 7, 4),
            "dropping the lease frees exactly one slot"
        );
    }
}
