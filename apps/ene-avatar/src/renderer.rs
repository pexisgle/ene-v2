use bevy::{
    app::{AnimationSystems, PluginsState},
    asset::LoadState,
    asset::io::{AssetSourceBuilder, file::FileAssetReader},
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
        let motion_path = settings.motion.as_deref().map(Path::new);
        let valid_motion = match motion_path {
            Some(path) => match crate::motion::validate(path) {
                Ok(()) => true,
                Err(error) => {
                    crate::emit(ene_shared::avatar::AvatarEvent::MotionError {
                        message: format!("モーションを読めません: {error}"),
                    });
                    false
                }
            },
            None => false,
        };
        if valid_motion && let Some(parent) = motion_path.and_then(Path::parent) {
            let parent = parent.to_owned();
            app.register_asset_source(
                "motion",
                AssetSourceBuilder::new(move || Box::new(FileAssetReader::new(parent.clone()))),
            );
        }
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
        let motion = if valid_motion {
            let name = motion_path
                .unwrap()
                .file_name()
                .ok_or("モーション名がありません")?
                .to_string_lossy();
            let handle = app
                .world()
                .resource::<AssetServer>()
                .load(format!("motion://{name}"));
            let entity = app
                .world_mut()
                .spawn((VrmaHandle(handle.clone()), ChildOf(model)))
                .id();
            Some((handle, entity))
        } else {
            None
        };
        while app.plugins_state() != PluginsState::Ready {
            bevy::tasks::tick_global_task_pools_on_main_thread();
            std::thread::yield_now();
        }
        app.finish();
        app.cleanup();
        Ok(Self {
            app,
            captures: Arc::new(Mutex::new(Capture::default())),
            handle,
            target,
            model,
            motion,
            motion_started: false,
            motion_failed: false,
            motion_started_at: std::time::Instant::now(),
            motion_state: if valid_motion {
                MotionState::Loading
            } else {
                MotionState::None
            },
            center: None,
            distance: 0.,
            fitted: false,
            generation: 0,
            capture_sequence: 0,
        })
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
    pub fn load_state(&self) -> Option<LoadState> {
        self.app
            .world()
            .resource::<AssetServer>()
            .get_load_state(self.handle.id())
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::time::TimeUpdateStrategy;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "requires GPU, cached Seed-san and the generated VRMA fixture; use just test-vrma"]
    fn vrma_retarget_stays_stationary_across_placement_and_pause() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");

        let model = std::env::var_os("ENE_VRMA_TEST_MODEL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
                    .join(".cache/ene/models/Seed-san.vrm")
            });
        let mut reference: Option<(Vec<f32>, Vec<Quat>)> = None;
        for (name, placement) in [
            ("anchor-and-silhouette.vrma", Transform::IDENTITY),
            (
                "anchor-and-silhouette.vrma",
                Transform::from_xyz(2., 3., -4.)
                    .with_rotation(Quat::from_rotation_y(0.8))
                    .with_scale(Vec3::splat(1.3)),
            ),
            ("rotated-scaled-parent.vrma", Transform::IDENTITY),
        ] {
            let motion = root.join("target/vrma-fixtures").join(name);
            let mut settings = AvatarSettings {
                model: model.to_string_lossy().into_owned(),
                motion: Some(motion.to_string_lossy().into_owned()),
                ..default()
            };
            let mut renderer = Renderer::new(360, 480, 1, &settings).unwrap();
            renderer
                .app
                .world_mut()
                .entity_mut(renderer.model)
                .insert(placement);
            renderer
                .app
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                    1. / 60.,
                )));
            let start = Instant::now();
            while !renderer.motion_started {
                assert!(
                    start.elapsed() < Duration::from_secs(90),
                    "VRMA initialization timeout"
                );
                renderer.update_motion(&settings);
                renderer.app.update();
                assert!(renderer.load_error().is_none());
                assert!(!renderer.motion_failed);
                renderer.ready();
            }
            // The imported model's world placement must not alter the local
            // retargeted pose or the height scaling used by VRMA.
            let hips = **renderer
                .app
                .world()
                .get::<HipsBoneEntity>(renderer.model)
                .unwrap();
            let arm = **renderer
                .app
                .world()
                .get::<LeftUpperArmBoneEntity>(renderer.model)
                .unwrap();
            let rest = renderer
                .app
                .world()
                .get::<RestTransform>(hips)
                .unwrap()
                .translation;
            let center = renderer.center;
            assert_eq!(
                center,
                Some(
                    renderer
                        .app
                        .world()
                        .get::<RestGlobalTransform>(hips)
                        .unwrap()
                        .translation()
                )
            );
            let mut heights = Vec::new();
            let mut turns = Vec::new();
            for _ in 0..300 {
                renderer.update_motion(&settings);
                renderer.app.update();
                renderer.ready();
                let pose = renderer.app.world().get::<Transform>(hips).unwrap();
                assert!((pose.translation.x - rest.x).abs() < 1e-5);
                assert!((pose.translation.z - rest.z).abs() < 1e-5);
                assert_eq!(
                    renderer.center, center,
                    "camera followed the moving silhouette"
                );
                assert_eq!(
                    *renderer
                        .app
                        .world()
                        .get::<Transform>(renderer.model)
                        .unwrap(),
                    placement
                );
                heights.push(pose.translation.y);
                turns.push(renderer.app.world().get::<Transform>(arm).unwrap().rotation);
            }
            assert!(
                heights.iter().copied().fold(f32::NEG_INFINITY, f32::max)
                    - heights.iter().copied().fold(f32::INFINITY, f32::min)
                    > 0.05,
                "vertical motion lost"
            );
            assert!(
                turns.iter().any(|q| q.angle_between(turns[0]) > 0.3),
                "arm animation lost"
            );
            if let Some((previous_heights, previous_turns)) = &reference {
                for (a, b) in heights.iter().zip(previous_heights) {
                    assert!((a - b).abs() < 1e-4, "placement changed retarget height");
                }
                for (a, b) in turns.iter().zip(previous_turns) {
                    assert!(
                        a.angle_between(*b) < 0.002,
                        "placement changed retarget rotation"
                    );
                }
            } else {
                reference = Some((heights, turns));
            }
            settings.motion_playing = false;
            renderer.update_motion(&settings);
            renderer.app.update();
            let paused = *renderer.app.world().get::<Transform>(hips).unwrap();
            let paused_arm = *renderer.app.world().get::<Transform>(arm).unwrap();
            for _ in 0..20 {
                renderer.update_motion(&settings);
                renderer.app.update();
            }
            assert_eq!(
                *renderer.app.world().get::<Transform>(hips).unwrap(),
                paused
            );
            assert_eq!(
                *renderer.app.world().get::<Transform>(arm).unwrap(),
                paused_arm
            );
            renderer.drain_capture();
        }
    }
}
