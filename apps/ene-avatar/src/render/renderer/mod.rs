use bevy::{
    app::{AnimationSystems, PluginsState},
    asset::{AssetPath, LoadState, UnapprovedPathMode},
    camera::RenderTarget,
    core_pipeline::tonemapping::Tonemapping,
    prelude::*,
    render::{
        RenderApp, RenderPlugin,
        render_asset::RenderAssets,
        settings::{RenderCreation, WgpuSettings},
        texture::GpuImage,
    },
    transform::TransformSystems,
    window::{
        ExitCondition, RawHandleWrapper, RawHandleWrapperHolder, WindowPlugin, WindowWrapper,
    },
};
use bevy_vrm1::prelude::*;
use ene_ipc::renderer::{AvatarConfig, AvatarId, MotionState};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Component)]
struct AvatarCamera;
#[derive(Component)]
struct MotionLoaded;

pub struct Renderer {
    pub app: App,
    handle: Handle<VrmAsset>,
    target: Handle<Image>,
    model: Entity,
    model_path: String,
    pub model_failed: bool,
    pub credit: String,
    motion: Option<(Handle<VrmaAsset>, Entity)>,
    motion_path: Option<String>,
    motion_started: bool,
    motion_failed: bool,
    motion_started_at: std::time::Instant,
    pub motion_state: MotionState,
    center: Option<Vec3>,
    distance: f32,
    body_height: f32,
    fitted: bool,
    pub generation: u64,
}
mod animation;
mod model;

impl Renderer {
    pub fn new(
        width: u32,
        height: u32,
        scale: u32,
        settings: &AvatarConfig,
        native_window: crate::wayland::NativeWindow,
        id: AvatarId,
        output: &crate::ipc::Output,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut app = App::new();
        let window = WindowWrapper::new(native_window);
        let wrapper = RawHandleWrapper::new(&window)?;
        let resources = bevy::tasks::block_on(bevy::render::renderer::initialize_renderer(
            {
                #[cfg(unix)]
                {
                    bevy::render::settings::Backends::VULKAN
                }
                #[cfg(windows)]
                {
                    bevy::render::settings::Backends::DX12
                }
            },
            Some(RawHandleWrapperHolder(Arc::new(Mutex::new(Some(wrapper))))),
            &WgpuSettings {
                force_fallback_adapter: std::env::var_os("ENE_AVATAR_CPU_GPU").is_some(),
                ..default()
            },
        ));
        app.add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .set(local_asset_plugin())
                .set(RenderPlugin {
                    render_creation: RenderCreation::Manual(resources),
                    ..default()
                })
                .disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>()
                .disable::<bevy::log::LogPlugin>(),
        );
        app.add_plugins((VrmPlugin, VrmaPlugin));
        app.add_observer(|event: On<LoadedVrma>, mut commands: Commands| {
            commands.entity(event.vrma).insert(MotionLoaded);
        });
        app.add_systems(
            PostUpdate,
            crate::render::motion::apply_hips_translation
                .after(AnimationSystems)
                .before(TransformSystems::Propagate)
                .before(VrmSystemSets::Constraints),
        );
        let mut target_image = Image::new_target_texture(
            width * scale,
            height * scale,
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            None,
        );
        target_image.texture_descriptor.usage |=
            bevy::render::render_resource::TextureUsages::COPY_SRC;
        target_image.data = None;
        target_image.copy_on_resize = false;
        let target = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(target_image);
        app.insert_resource(ClearColor(Color::NONE));
        app.world_mut().spawn((
            AvatarCamera,
            Camera3d::default(),
            Camera {
                clear_color: ClearColorConfig::Custom(Color::NONE),
                ..default()
            },
            RenderTarget::Image(target.clone().into()),
            Projection::Perspective(PerspectiveProjection {
                aspect_ratio: width as f32 / height as f32,
                ..default()
            }),
            Tonemapping::None,
            Msaa::Off,
            Transform::from_xyz(0.0, 0.85, 3.0).looking_at(Vec3::new(0.0, 0.85, 0.0), Vec3::Y),
        ));
        app.world_mut().spawn((
            DirectionalLight {
                illuminance: 10000.0,
                ..default()
            },
            Transform::from_xyz(1.0, 2.0, 3.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        ));
        app.insert_resource(GlobalAmbientLight {
            color: Color::WHITE,
            brightness: 300.0,
            ..default()
        });
        let model = app.world_mut().spawn_empty().id();
        while app.plugins_state() != PluginsState::Ready {
            bevy::tasks::tick_global_task_pools_on_main_thread();
            std::thread::yield_now();
        }
        app.finish();
        app.cleanup();
        let mut renderer = Self {
            app,
            handle: Handle::default(),
            target,
            model,
            model_path: String::new(),
            model_failed: true,
            credit: String::new(),
            motion: None,
            motion_path: None,
            motion_started: false,
            motion_failed: false,
            motion_started_at: std::time::Instant::now(),
            motion_state: MotionState::None,
            center: None,
            distance: 0.,
            body_height: 0.,
            fitted: false,
            generation: 0,
        };
        if let Err(message) = renderer.configure(settings) {
            output.avatar_error(id, message);
        }
        Ok(renderer)
    }
    /// Returns true only when a replacement model starts loading.
    pub fn configure(&mut self, settings: &AvatarConfig) -> Result<bool, String> {
        if self.model_path != settings.model || self.model_failed {
            self.set_model(settings)?;
            return Ok(true);
        }
        if self.motion_path != settings.motion || self.motion_failed {
            self.set_motion(settings.motion.as_deref());
        }
        Ok(false)
    }
    pub fn resize(&mut self, width: u32, height: u32, scale: u32) {
        self.generation += 1;
        let size = bevy::render::render_resource::Extent3d {
            width: width * scale,
            height: height * scale,
            depth_or_array_layers: 1,
        };
        let mut images = self.app.world_mut().resource_mut::<Assets<Image>>();
        if images.get(&self.target).unwrap().texture_descriptor.size != size {
            images.get_mut(&self.target).unwrap().resize(size);
        }
        if let Ok(mut projection) = self
            .app
            .world_mut()
            .query_filtered::<&mut Projection, With<AvatarCamera>>()
            .single_mut(self.app.world_mut())
            && let Projection::Perspective(p) = &mut *projection
        {
            p.aspect_ratio = width as f32 / height as f32;
        }
    }
    pub fn rendered_texture(&self) -> Option<wgpu::Texture> {
        self.app
            .get_sub_app(RenderApp)?
            .world()
            .resource::<RenderAssets<GpuImage>>()
            .get(&self.target)
            .map(|image| (*image.texture).clone())
    }
}

fn local_asset_plugin() -> AssetPlugin {
    AssetPlugin {
        // Retain drive/UNC prefixes. Only individually validated model/motion loads
        // override the default refusal of paths outside the asset directory.
        file_path: String::new(),
        unapproved_path_mode: UnapprovedPathMode::Deny,
        ..default()
    }
}

fn local_path(path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(std::fs::canonicalize(path)?)
}

#[cfg(test)]
mod tests;
