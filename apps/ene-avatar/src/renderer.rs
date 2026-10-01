use bevy::{
    app::{AnimationSystems, PluginsState},
    asset::io::{AssetSourceBuilder, file::FileAssetReader},
    asset::{AssetPath, LoadState},
    camera::RenderTarget,
    core_pipeline::tonemapping::Tonemapping,
    prelude::*,
    render::{
        RenderPlugin,
        settings::{RenderCreation, WgpuSettings},
        view::screenshot::{Screenshot, ScreenshotCaptured},
    },
    transform::TransformSystems,
    window::{ExitCondition, WindowPlugin},
};
use bevy_vrm1::prelude::*;
use ene_shared::avatar::{AvatarSettings, MotionState};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

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
    motion: Option<(Handle<VrmaAsset>, Entity)>,
    motion_started: bool,
    motion_failed: bool,
    motion_started_at: std::time::Instant,
    pub motion_state: MotionState,
    center: Option<Vec3>,
    distance: f32,
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
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let path = Path::new(&settings.model);
        let mut app = App::new();
        // Sources are registered before AssetPlugin. A fixed filesystem source
        // lets later selections use another directory without rebuilding Bevy.
        // Only validated, embedded VRMA files are passed to this reader.
        app.register_asset_source(
            "motion",
            AssetSourceBuilder::new(|| Box::new(FileAssetReader::new("/"))),
        );
        let parent = path.parent().ok_or("モデルのフォルダがありません")?;
        app.add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .set(AssetPlugin {
                    file_path: parent.to_string_lossy().into_owned(),
                    ..default()
                })
                .set(RenderPlugin {
                    render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                        force_fallback_adapter: std::env::var_os("ENE_AVATAR_CPU_GPU").is_some(),
                        ..default()
                    })),
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
        let target =
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .add(Image::new_target_texture(
                    width * scale,
                    height * scale,
                    bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
                    None,
                ));
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
        let handle = app.world().resource::<AssetServer>().load(
            path.file_name()
                .ok_or("モデル名がありません")?
                .to_string_lossy()
                .into_owned(),
        );
        let model = app.world_mut().spawn(VrmHandle(handle.clone())).id();
        while app.plugins_state() != PluginsState::Ready {
            bevy::tasks::tick_global_task_pools_on_main_thread();
            std::thread::yield_now();
        }
        app.finish();
        app.cleanup();
        let mut renderer = Self {
            app,
            captures: Arc::new(Mutex::new(Capture::default())),
            handle,
            target,
            model,
            motion: None,
            motion_started: false,
            motion_failed: false,
            motion_started_at: std::time::Instant::now(),
            motion_state: MotionState::None,
            center: None,
            distance: 0.,
            fitted: false,
            generation: 0,
            capture_sequence: 0,
        };
        renderer.set_motion(settings.motion.as_deref());
        Ok(renderer)
    }
    pub fn set_motion(&mut self, path: Option<&str>) {
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
            let path = std::fs::canonicalize(path)?;
            crate::motion::validate(&path)?;
            Ok(path.strip_prefix("/")?.to_path_buf())
        })();
        match result {
            Ok(path) => {
                let handle = world
                    .resource::<AssetServer>()
                    .load(AssetPath::from(path).with_source("motion"));
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
    pub fn resize(&mut self, width: u32, height: u32, scale: u32) {
        self.generation += 1;
        self.app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .get_mut(&self.target)
            .unwrap()
            .resize(bevy::render::render_resource::Extent3d {
                width: width * scale,
                height: height * scale,
                depth_or_array_layers: 1,
            });
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
    pub fn capture(&mut self) {
        let shared = self.captures.clone();
        let generation = self.generation;
        {
            let mut s = shared.lock().unwrap();
            if s.pending >= 4 {
                return;
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
    }
    pub fn ready(&mut self) -> bool {
        let world = self.app.world_mut();
        let initialized = world.get::<Initialized>(self.model).is_some();
        if initialized {
            use bevy::camera::primitives::Aabb;
            let mut minimum = Vec3::splat(f32::INFINITY);
            let mut maximum = Vec3::splat(f32::NEG_INFINITY);
            // glTF uses DynamicSkinnedMeshBounds: Aabb follows skinned geometry,
            // not a bind-pose box which can cull hands during a motion.
            for (bounds, transform) in world
                .query_filtered::<(&Aabb, &GlobalTransform), With<Mesh3d>>()
                .iter(world)
            {
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
                let target = *self.center.get_or_insert(anchor);
                if let Ok((projection, mut transform)) = world
                    .query_filtered::<(&Projection, &mut Transform), With<AvatarCamera>>()
                    .single_mut(world)
                    && let Projection::Perspective(p) = projection
                {
                    let tangent = (p.fov * 0.5).tan();
                    let half = (maximum - target).abs().max((minimum - target).abs());
                    let needed =
                        (half.y / tangent).max(half.x / (tangent * p.aspect_ratio)) * 1.2 + half.z;
                    // A one-way envelope avoids per-frame zoom oscillation.
                    if !self.fitted || needed > self.distance * 1.01 {
                        self.distance = (needed * 1.1).max(self.distance).max(0.1);
                        *transform = Transform::from_translation(target + Vec3::Z * self.distance)
                            .looking_at(target, Vec3::Y);
                        self.fitted = true;
                        self.generation += 1;
                    }
                }
            }
        }
        initialized && self.fitted
    }
    pub fn update_motion(&mut self, settings: &AvatarSettings) {
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
    pub fn load_error(&self) -> Option<String> {
        match self
            .app
            .world()
            .resource::<AssetServer>()
            .get_load_state(self.handle.id())
        {
            Some(LoadState::Failed(error)) => Some(format!("VRMを読み込めません: {error}")),
            _ => None,
        }
    }
}
