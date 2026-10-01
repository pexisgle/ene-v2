use bevy::{
    app::{AnimationSystems, PluginsState},
    asset::{AssetPath, LoadState},
    camera::RenderTarget,
    core_pipeline::tonemapping::Tonemapping,
    prelude::*,
    render::{
        RenderApp, RenderPlugin,
        render_asset::RenderAssets,
        settings::{RenderCreation, WgpuSettings},
        texture::GpuImage,
        view::screenshot::{Screenshot, ScreenshotCaptured},
    },
    transform::TransformSystems,
    window::{
        ExitCondition, RawHandleWrapper, RawHandleWrapperHolder, WindowPlugin, WindowWrapper,
    },
};
use bevy_vrm1::prelude::*;
use ene_shared::avatar::{AvatarSettings, MotionState};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Resource, Default)]
struct ProfileFrame {
    sequence: u64,
    token: Option<usize>,
}

fn begin_scene_profile(
    mut timer: ResMut<crate::telemetry::SceneTimer>,
    mut frame: ResMut<ProfileFrame>,
    mut pending: ResMut<bevy::render::renderer::PendingCommandBuffers>,
) {
    frame.token = timer.begin_buffered(frame.sequence, &mut pending);
}
fn finish_scene_profile(
    mut timer: ResMut<crate::telemetry::SceneTimer>,
    frame: Res<ProfileFrame>,
    mut pending: ResMut<bevy::render::renderer::PendingCommandBuffers>,
) {
    if let Some(token) = frame.token {
        timer.finish_buffered(token, &mut pending);
    }
}
fn map_scene_profile(
    mut timer: ResMut<crate::telemetry::SceneTimer>,
    mut frame: ResMut<ProfileFrame>,
) {
    if let Some(token) = frame.token.take() {
        timer.map_after_submit(token);
    }
}

#[derive(Default)]
pub struct Capture {
    pub revision: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub pending: u32,
}
#[derive(Component)]
struct AvatarCamera;
#[derive(Component)]
struct MotionLoaded;

pub struct Renderer {
    pub app: App,
    pub captures: Arc<Mutex<Capture>>,
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
    capture_sequence: u64,
}
impl Renderer {
    pub fn new(
        width: u32,
        height: u32,
        scale: u32,
        settings: &AvatarSettings,
        native_window: crate::native::NativeWindow,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut app = App::new();
        let window = WindowWrapper::new(native_window);
        let wrapper = RawHandleWrapper::new(&window)?;
        let resources = bevy::tasks::block_on(bevy::render::renderer::initialize_renderer(
            bevy::render::settings::Backends::VULKAN,
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
                .set(AssetPlugin {
                    // Load validated embedded files from any selected folder
                    // without rebuilding the asset server or GPU environment.
                    file_path: "/".into(),
                    ..default()
                })
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
            crate::motion::keep_stationary
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
        if std::env::var("ENE_AVATAR_PROFILE").as_deref() == Ok("1") {
            let world = app.world();
            let timer = crate::telemetry::SceneTimer::new(
                world
                    .resource::<bevy::render::renderer::RenderDevice>()
                    .clone(),
                world
                    .resource::<bevy::render::renderer::RenderQueue>()
                    .clone(),
            );
            if let Some(reason) = timer.unavailable_reason() {
                eprintln!("avatar scene timing unavailable: {reason}");
            }
            use bevy::render::renderer::{RenderGraph, RenderGraphSystems};
            app.sub_app_mut(RenderApp)
                .insert_resource(timer)
                .init_resource::<ProfileFrame>()
                .add_systems(
                    RenderGraph,
                    (
                        begin_scene_profile.in_set(RenderGraphSystems::Begin),
                        finish_scene_profile
                            .after(RenderGraphSystems::Render)
                            .before(RenderGraphSystems::Submit),
                        map_scene_profile.in_set(RenderGraphSystems::Finish),
                    ),
                );
        }
        let mut renderer = Self {
            app,
            captures: Arc::new(Mutex::new(Capture::default())),
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
            capture_sequence: 0,
        };
        if let Err(message) = renderer.configure(settings) {
            crate::emit(ene_shared::avatar::AvatarEvent::Error { message });
        }
        Ok(renderer)
    }
    /// Returns true only when a replacement model starts loading.
    pub fn configure(&mut self, settings: &AvatarSettings) -> Result<bool, String> {
        if self.model_path != settings.model || self.model_failed {
            self.set_model(settings)?;
            return Ok(true);
        }
        if self.motion_path != settings.motion || self.motion_failed {
            self.set_motion(settings.motion.as_deref());
        }
        Ok(false)
    }
    fn set_model(&mut self, settings: &AvatarSettings) -> Result<(), String> {
        let path = local_path(&settings.model).map_err(|error| {
            format!("VRMを読めません: {error}。設定でモデルを選び直してください。")
        })?;
        let credit = crate::model::validate(&Path::new("/").join(&path))
            .map_err(|error| error.to_string())?;
        // Validate before removing the current model. A bad selection leaves
        // the existing model usable and can be corrected through Configure.
        let asset_path = AssetPath::from(path);
        let server = self.app.world().resource::<AssetServer>();
        if self.model_failed && self.model_path == settings.model {
            server.reload(asset_path.clone());
        }
        let handle = server.load(asset_path);
        self.set_motion(None);
        let world = self.app.world_mut();
        world.despawn(self.model);
        self.model = world.spawn(VrmHandle(handle.clone())).id();
        self.handle = handle;
        self.model_path = settings.model.clone();
        self.model_failed = false;
        self.credit = credit;
        self.center = None;
        self.distance = 0.;
        self.body_height = 0.;
        self.fitted = false;
        self.set_motion(settings.motion.as_deref());
        Ok(())
    }
    pub fn set_motion(&mut self, path: Option<&str>) {
        let retry = self.motion_failed && self.motion_path.as_deref() == path;
        self.motion_path = path.map(str::to_owned);
        let world = self.app.world_mut();
        if let Some((_, entity)) = self.motion.take() {
            // ChildOf uses linked despawning: the old skeleton is removed too.
            world.despawn(entity);
        }
        let players = world
            .query::<(
                Entity,
                &mut AnimationPlayer,
                Option<&mut AnimationTransitions>,
            )>()
            .iter_mut(world)
            .map(|(entity, mut player, transitions)| {
                player.stop_all();
                if let Some(mut transitions) = transitions {
                    *transitions = AnimationTransitions::default();
                }
                entity
            })
            .collect::<Vec<_>>();
        for entity in players {
            world.entity_mut(entity).remove::<AnimationGraphHandle>();
        }
        if let Some(hips) = world.get::<HipsBoneEntity>(self.model).map(|hips| **hips) {
            world.entity_mut(hips).remove::<crate::motion::MotionAxes>();
        }
        for (rest, mut pose) in world
            .query::<(&RestTransform, &mut Transform)>()
            .iter_mut(world)
        {
            *pose = rest.0;
        }
        let expressions = world
            .get::<ExpressionEntityMap>(self.model)
            .map(|map| map.0.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for expression in expressions {
            if let Some(mut pose) = world.get_mut::<Transform>(expression) {
                pose.translation.x = 0.;
            }
        }
        world.resource_mut::<Time<Virtual>>().unpause();
        self.motion_started = false;
        self.motion_failed = false;
        self.motion_started_at = std::time::Instant::now();
        self.motion_state = MotionState::None;
        // Reject readbacks of the previous pose; retain the displayed frame and
        // its input region until a matching new frame is available.
        self.generation += 1;
        let Some(path) = path else { return };
        let result = (|| -> Result<_, Box<dyn std::error::Error>> {
            let path = local_path(path)?;
            crate::motion::validate(&Path::new("/").join(&path))?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                let asset_path = AssetPath::from(path);
                let server = world.resource::<AssetServer>();
                if retry {
                    server.reload(asset_path.clone());
                }
                let handle = server.load(asset_path);
                let entity = world
                    .spawn((VrmaHandle(handle.clone()), ChildOf(self.model)))
                    .id();
                self.motion = Some((handle, entity));
                self.motion_state = MotionState::Loading;
            }
            Err(error) => {
                self.motion_failed = true;
                crate::emit(ene_shared::avatar::AvatarEvent::MotionError {
                    message: format!("モーションを読めません: {error}"),
                });
            }
        }
    }
    pub fn drain_capture(&mut self) {
        // Deliver an already submitted readback before dropping its receiver.
        // No new captures are scheduled during this bounded shutdown drain.
        let started = std::time::Instant::now();
        while self.captures.lock().unwrap().pending > 0
            && started.elapsed() < std::time::Duration::from_millis(250)
        {
            self.app.update();
            std::thread::yield_now();
        }
    }
    pub fn resume(&mut self) {
        // Drop the wall-clock gap so hidden time never advances animation or
        // spring physics when updates start again.
        let world = self.app.world_mut();
        if let Some(receiver) = world.get_resource::<bevy::time::TimeReceiver>() {
            for _ in receiver.0.try_iter() {}
        }
        world
            .resource_mut::<Time<Real>>()
            .update_with_instant(bevy::platform::time::Instant::now());
        self.motion_started_at = std::time::Instant::now();
        self.generation += 1;
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
    pub fn capture(&mut self) -> Option<u64> {
        let shared = self.captures.clone();
        let generation = self.generation;
        {
            let mut s = shared.lock().unwrap();
            if s.pending >= 4 {
                return None;
            }
            s.pending += 1;
        }
        self.capture_sequence += 1;
        let sequence = self.capture_sequence;
        self.app
            .world_mut()
            .spawn(Screenshot::image(self.target.clone()))
            .observe(move |event: On<ScreenshotCaptured>| {
                let mut shared = shared.lock().unwrap();
                shared.pending -= 1;
                // Readbacks can complete out of order. Never present an older
                // pose after a newer one, even across pause/resize boundaries.
                if sequence <= shared.revision {
                    return;
                }
                if let Ok(image) = event.image.clone().try_into_dynamic() {
                    let image = image.to_rgba8();
                    shared.width = image.width();
                    shared.height = image.height();
                    shared.rgba = image.into_raw();
                    shared.generation = generation;
                    shared.revision = sequence;
                }
            });
        Some(sequence)
    }
    pub fn profile_frame(&mut self, sequence: u64) {
        if let Some(mut frame) = self
            .app
            .sub_app_mut(RenderApp)
            .world_mut()
            .get_resource_mut::<ProfileFrame>()
        {
            frame.sequence = sequence;
        }
    }
    pub fn poll_scene_timings(&mut self) -> Vec<crate::telemetry::SceneTimestampSample> {
        if let Some(mut timer) = self
            .app
            .sub_app_mut(RenderApp)
            .world_mut()
            .get_resource_mut::<crate::telemetry::SceneTimer>()
        {
            let samples = timer.poll();
            for error in timer.take_errors() {
                eprintln!("avatar scene timing: {error}");
            }
            samples
        } else {
            Vec::new()
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
    pub fn ready(&mut self, avatar_height: f32, viewport_height: u32) -> bool {
        if self.model_failed {
            return false;
        }
        let world = self.app.world_mut();
        let initialized = world.get::<Initialized>(self.model).is_some();
        if initialized && !self.fitted {
            use bevy::camera::primitives::MeshAabb;
            let mut minimum = Vec3::splat(f32::INFINITY);
            let mut maximum = Vec3::splat(f32::NEG_INFINITY);
            let mut query =
                world.query::<(&Mesh3d, &GlobalTransform, Option<&RestGlobalTransform>)>();
            let meshes = world.resource::<Assets<Mesh>>();
            // Frame the undeformed mesh once. Conservative skinned bounds
            // remain enabled for culling, but must not shrink the displayed body.
            for (mesh, global, rest) in query.iter(world) {
                let Some(bounds) = meshes.get(&mesh.0).and_then(MeshAabb::compute_aabb) else {
                    continue;
                };
                let transform = rest.map(|rest| &rest.0).unwrap_or(global);
                let center: Vec3 = bounds.center.into();
                let half: Vec3 = bounds.half_extents.into();
                for x in [-1., 1.] {
                    for y in [-1., 1.] {
                        for z in [-1., 1.] {
                            let corner =
                                transform.transform_point(center + half * Vec3::new(x, y, z));
                            minimum = minimum.min(corner);
                            maximum = maximum.max(corner);
                        }
                    }
                }
            }
            if minimum.is_finite() && maximum.is_finite() {
                // Fix the camera's axis to the rest pose. Never chase the
                // current silhouette or move the model to its bounding center.
                let anchor = world
                    .get::<HipsBoneEntity>(self.model)
                    .and_then(|hips| world.get::<RestGlobalTransform>(**hips))
                    .map(|rest| rest.translation())
                    .unwrap_or((minimum + maximum) * 0.5);
                self.center = Some(anchor);
                let half = (maximum - anchor).abs().max((minimum - anchor).abs());
                self.distance = (half.length() * 3.).max(1.);
                self.body_height = (maximum.y - minimum.y).max(0.01);
                self.fitted = true;
            }
        }
        if self.fitted
            && let Ok((mut projection, mut transform)) = world
                .query_filtered::<(&mut Projection, &mut Transform), With<AvatarCamera>>()
                .single_mut(world)
            && let Projection::Perspective(p) = &mut *projection
        {
            let fov = 2.
                * (self.body_height * viewport_height as f32
                    / (2. * avatar_height * self.distance))
                    .atan();
            let target = self.center.unwrap();
            let translation = target + Vec3::Z * self.distance;
            if (p.fov - fov).abs() > 0.00001 || transform.translation != translation {
                p.fov = fov;
                *transform = Transform::from_translation(translation).looking_at(target, Vec3::Y);
                self.generation += 1;
            }
        }
        initialized && self.fitted
    }
    pub fn update_motion(&mut self, settings: &AvatarSettings) {
        if self.model_failed {
            return;
        }
        let Some((handle, entity)) = self.motion.as_ref() else {
            return;
        };
        if self.motion_failed {
            return;
        }
        let world = self.app.world_mut();
        if !self.motion_started {
            if let Some(LoadState::Failed(error)) =
                world.resource::<AssetServer>().get_load_state(handle.id())
            {
                crate::emit(ene_shared::avatar::AvatarEvent::MotionError {
                    message: format!("VRMAを読めません: {error}"),
                });
                self.motion_failed = true;
                self.motion_state = MotionState::None;
                return;
            }
            if self.motion_started_at.elapsed() > std::time::Duration::from_secs(90) {
                crate::emit(ene_shared::avatar::AvatarEvent::MotionError {
                    message: "モーションの読み込みが90秒以内に完了しませんでした。".into(),
                });
                self.motion_failed = true;
                self.motion_state = MotionState::None;
                return;
            }
            if self.fitted && world.get::<MotionLoaded>(*entity).is_some() {
                let Some((hips, axes)) =
                    crate::motion::MotionAxes::from_world(world, self.model, *entity)
                else {
                    crate::emit(ene_shared::avatar::AvatarEvent::MotionError {
                        message: "モーションの骨格の基準を取得できません。".into(),
                    });
                    self.motion_failed = true;
                    self.motion_state = MotionState::None;
                    return;
                };
                world.entity_mut(hips).insert(axes);
                world.trigger(PlayVrma {
                    vrma: *entity,
                    repeat: if settings.motion_loop {
                        bevy::animation::RepeatAnimation::Forever
                    } else {
                        bevy::animation::RepeatAnimation::Never
                    },
                    transition_duration: std::time::Duration::ZERO,
                    reset_spring_bones: true,
                });
                self.motion_started = true;
                self.generation += 1;
            }
        }
        if self.motion_started {
            if settings.motion_playing
                && self.motion_state == MotionState::Paused
                && world
                    .query::<&AnimationPlayer>()
                    .iter(world)
                    .all(AnimationPlayer::all_finished)
            {
                world.trigger(PlayVrma {
                    vrma: *entity,
                    repeat: if settings.motion_loop {
                        bevy::animation::RepeatAnimation::Forever
                    } else {
                        bevy::animation::RepeatAnimation::Never
                    },
                    transition_duration: std::time::Duration::ZERO,
                    reset_spring_bones: true,
                });
                self.generation += 1;
            }
            for mut player in world.query::<&mut AnimationPlayer>().iter_mut(world) {
                for (_, animation) in player.playing_animations_mut() {
                    if settings.motion_loop {
                        animation.set_repeat(bevy::animation::RepeatAnimation::Forever);
                    } else if animation.repeat_mode() == bevy::animation::RepeatAnimation::Forever {
                        animation.set_repeat(bevy::animation::RepeatAnimation::Count(
                            animation.completions().saturating_add(1),
                        ));
                    }
                }
                if settings.motion_playing {
                    player.resume_all();
                } else {
                    player.pause_all();
                }
            }
            // Pausing also freezes spring physics, keeping the visible pose
            // and the corresponding click silhouette stable.
            if settings.motion_playing {
                world.resource_mut::<Time<Virtual>>().unpause();
            } else {
                world.resource_mut::<Time<Virtual>>().pause();
            }
            let previous_state = self.motion_state;
            self.motion_state = if !settings.motion_playing {
                MotionState::Paused
            } else if world
                .query::<&AnimationPlayer>()
                .iter(world)
                .all(AnimationPlayer::all_finished)
            {
                MotionState::Finished
            } else {
                MotionState::Playing
            };
            if (previous_state == MotionState::Paused) != (self.motion_state == MotionState::Paused)
            {
                self.generation += 1;
            }
        }
    }
    pub fn load_error(&mut self) -> Option<String> {
        if self.model_failed {
            return None;
        }
        match self
            .app
            .world()
            .resource::<AssetServer>()
            .get_load_state(self.handle.id())
        {
            Some(LoadState::Failed(error)) => {
                self.model_failed = true;
                Some(format!(
                    "VRMを読み込めません: {error}。設定でモデルを選び直してください。"
                ))
            }
            _ => None,
        }
    }
}

fn local_path(path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = std::fs::canonicalize(path)?;
    Ok(path.strip_prefix("/")?.to_path_buf())
}
