use super::GpuError;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::sync::Arc;

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
