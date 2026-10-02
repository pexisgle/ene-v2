//! GPU-only frame transport on Bevy's existing device and queue.
//!
//! Call `enqueue` on the main thread after `renderer.app.update()`, using the
//! final `GpuImage` from `RenderApp`. Width/height are physical pixels; runs
//! are logical pixels. The final image always supplies the exact GPU mask.
//! Poll each tick, discard stale generations/sequences in the caller, then draw and install
//! that frame's input region before presenting the returned surface texture.
//!
//! Keep the complete `Frame` alive while displaying/reusing its texture. Its
//! lease prevents ring reuse. A bare cloned `wgpu::Texture` does not hold that
//! lease. Three occupied or leased slots make `enqueue` return false; callers
//! should skip that capture and poll again, never block the render loop.
use crate::mask::Run;
use bevy::render::renderer::{RenderDevice, RenderQueue};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{
    fmt,
    sync::{Arc, mpsc},
};
use wgpu::util::DeviceExt;

pub const RING_SIZE: usize = 3;
// Keep in sync with gpu_mask.wgsl. Adjacent pixels share SIMD lanes.
const MASK_PIXELS_PER_WORKGROUP: u32 = 256;
pub const DEFAULT_RUN_CAPACITY: u32 = 4096;
const HEADER_BYTES: u64 = 16;
const RUN_BYTES: u64 = 16;

#[derive(Debug, Clone)]
pub struct GpuError(pub String);
impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for GpuError {}

#[derive(Debug, Clone)]
pub struct Frame {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub generation: u64,
    pub sequence: u64,
    pub runs: Vec<Run>,
    _lease: Arc<()>,
}

struct Pending {
    width: u32,
    height: u32,
    scale: u32,
    generation: u64,
    sequence: u64,
    receiver: Option<mpsc::Receiver<Result<(), String>>>,
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
        slot.pending = Some(Pending {
            width,
            height,
            scale,
            generation,
            sequence,
            receiver: None,
        });
        submit_slot(
            self.device.wgpu_device(),
            &self.queue,
            &self.pipeline,
            slot,
            Some(source),
        );
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
            let Some(pending) = slot.pending.take() else {
                continue;
            };
            match pending.receiver.as_ref().unwrap().try_recv() {
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
                    slot.pending = Some(pending);
                    submit_slot(
                        self.device.wgpu_device(),
                        &self.queue,
                        &self.pipeline,
                        slot,
                        None,
                    );
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
            frames.push(Frame {
                texture: slot.texture.as_ref().unwrap().clone(),
                width: pending.width,
                height: pending.height,
                scale: pending.scale,
                generation: pending.generation,
                sequence: pending.sequence,
                runs,
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
) {
    let pending = slot.pending.as_mut().unwrap();
    let texture = slot.texture.as_ref().unwrap();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("avatar frame copy/mask"),
    });
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
    {
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
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("avatar scan interval starts"),
                timestamp_writes: None,
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
    pending.receiver = Some(receiver);
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
        })
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
        draw_texture(&self.device, &self.queue, &self.pipeline, texture, &target);
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
fn draw_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    texture: &wgpu::Texture,
    target: &wgpu::TextureView,
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
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit([encoder.finish()]);
}
