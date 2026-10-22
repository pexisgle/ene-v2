use crate::character::{AvatarState, CharacterId, Config, Notification};
use bevy::{
    camera::{RenderTarget, ScalingMode, primitives::Aabb, visibility::VisibilitySystems},
    prelude::*,
    window::{CompositeAlphaMode, PresentMode, Window, WindowRef, WindowResolution},
};
use serde_json::json;

#[derive(Component, Clone, PartialEq)]
#[require(HitRegions, Transform, Visibility)]
pub struct Output {
    pub name: String,
    pub origin: Vec2,
    pub size: Vec2,
    pub units_per_logical_pixel: f32,
}

pub fn bounds(output: &Output) -> Rect {
    Rect::from_corners(output.origin, output.origin + output.size)
}

pub fn desktop_position(output: &Output, local: Vec2) -> Vec2 {
    output.origin + local * output.units_per_logical_pixel
}

/// The monitor a placement resolves to: the named one, or the first output.
pub fn pick_output<'a>(
    placement: &Config,
    mut outputs: impl Iterator<Item = &'a Output>,
) -> Option<&'a Output> {
    let first = outputs.next()?;
    std::iter::once(first)
        .chain(outputs)
        .find(|output| placement.monitor.as_deref() == Some(output.name.as_str()))
        .or(Some(first))
}

/// The requested logical position, or the default on the selected output.
pub fn resolved_position(placement: &Config, output: Option<&Output>) -> Option<Vec2> {
    placement.position.map(Vec2::from_array).or_else(|| {
        output.map(|output| output.size / output.units_per_logical_pixel * Vec2::new(0.5, 0.8))
    })
}

#[derive(Component, Default, PartialEq)]
pub struct HitRegions(pub Vec<(Entity, Rect)>);

#[derive(Resource, Default)]
pub struct Drag {
    pub entity: Option<Entity>,
    /// Desktop position where the press happened.
    start: Vec2,
    /// Pointer-to-foot offset in logical pixels, unchanged across monitor DPIs.
    offset: Vec2,
    moved: bool,
}

#[derive(Message)]
pub(crate) struct PointerInput {
    pub output: Entity,
    pub local: Vec2,
    pub button: Option<bool>,
}

pub fn setup(app: &mut App) {
    // 1 FPS needs a full second plus scheduling jitter before the stall cap.
    app.insert_resource(Time::<Virtual>::from_max_delta(
        std::time::Duration::from_secs(2),
    ));
    app.init_resource::<Drag>()
        .add_message::<PointerInput>()
        .add_systems(PreUpdate, pointer)
        .add_systems(Update, (cameras, place_avatars))
        .add_systems(Update, sync_virtual_time)
        .add_observer(|_: On<Remove, Output>, mut drag: ResMut<Drag>| drag.entity = None)
        .add_observer(|event: On<Remove, CharacterId>, mut drag: ResMut<Drag>| {
            if drag.entity == Some(event.entity) {
                drag.entity = None;
            }
        })
        .add_systems(
            PostUpdate,
            regions
                .after(VisibilitySystems::CalculateBounds)
                .after(VisibilitySystems::VisibilityPropagate)
                .before(VisibilitySystems::CheckVisibility),
        );
}

pub fn window(output: &Output) -> Window {
    let mut resolution = WindowResolution::new(output.size.x as u32, output.size.y as u32);
    resolution.set_scale_factor(output.units_per_logical_pixel);
    Window {
        title: "ene-avatar".into(),
        transparent: true,
        resolution,
        composite_alpha_mode: CompositeAlphaMode::PreMultiplied,
        present_mode: PresentMode::AutoNoVsync,
        ..default()
    }
}

fn cameras(mut commands: Commands, outputs: Query<(Entity, &Output), Changed<Output>>) {
    for (entity, output) in &outputs {
        let center = output.origin + output.size * 0.5;
        commands.entity(entity).insert((
            Camera3d::default(),
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: output.size.x,
                    height: output.size.y,
                },
                near: 0.0,
                far: 100_000.0,
                ..OrthographicProjection::default_3d()
            }),
            RenderTarget::Window(WindowRef::Entity(entity)),
            Transform::from_xyz(center.x, -center.y, 50_000.0),
            Msaa::Off,
            bevy::core_pipeline::tonemapping::Tonemapping::None,
        ));
    }
}

/// Derives placement and stacking depth; unchanged transforms stay untouched.
#[allow(clippy::type_complexity)]
fn place_avatars(
    outputs: Query<&Output>,
    mut avatars: Query<(Entity, &Config, &mut Transform), With<CharacterId>>,
) {
    for (index, (_, placement, mut transform)) in avatars
        .iter_mut()
        .sort_unstable_by_key::<Entity, _>(|entity| entity.to_bits())
        .enumerate()
    {
        let output = pick_output(placement, outputs.iter());
        let (position, units_per_logical_pixel) = output.map_or((Vec2::ZERO, 1.), |output| {
            (
                desktop_position(
                    output,
                    resolved_position(placement, Some(output)).unwrap_or_default(),
                ),
                output.units_per_logical_pixel,
            )
        });
        let next = Transform::from_xyz(position.x, -position.y, index as f32 * 1000.).with_scale(
            Vec3::splat(160. * placement.size_percent / 100. * units_per_logical_pixel),
        );
        transform.set_if_neq(next);
    }
}

fn regions(
    mut outputs: Query<(&Output, &mut Camera, &mut HitRegions)>,
    avatars: Query<(Entity, &AvatarState), With<CharacterId>>,
    parents: Query<&ChildOf>,
    meshes: Query<(Entity, &Aabb, &GlobalTransform, &InheritedVisibility), With<Mesh3d>>,
) {
    let geometry: Vec<_> = meshes
        .iter()
        .filter_map(|(entity, aabb, transform, visible)| {
            let (owner, state) = avatars
                .iter_many(std::iter::once(entity).chain(parents.iter_ancestors(entity)))
                .next()?;
            if *state != AvatarState::Ready || !visible.get() {
                return None;
            }
            // shortcut: mesh bounds include gaps and omit morph deformation; refine if input misses are observed.
            let matrix = transform.to_matrix();
            let center = matrix.transform_point3(aabb.center.into());
            let half = Mat3::from_mat4(matrix).abs() * Vec3::from(aabb.half_extents);
            Some((owner, Vec2::new(center.x, -center.y), half.truncate()))
        })
        .collect();
    for (output, mut camera, mut regions) in &mut outputs {
        let had = !regions.0.is_empty();
        let mut next = Vec::new();
        for &(actor, center, half) in &geometry {
            let center = (center - output.origin) / output.units_per_logical_pixel;
            let half = half / output.units_per_logical_pixel + Vec2::splat(2.);
            let limit = output.size / output.units_per_logical_pixel;
            let min = (center - half).max(Vec2::ZERO);
            let max = (center + half).min(limit);
            if min.is_finite() && max.is_finite() && min.cmplt(max).all() {
                next.push((actor, Rect::from_corners(min, max)));
            }
        }
        // Clear the previous image once before disabling an empty camera.
        let active = had || !next.is_empty();
        regions.set_if_neq(HitRegions(next));
        if camera.is_active != active {
            camera.is_active = active;
        }
    }
}

/// No output surfaces means no simulation; the Wayland runner only translates
/// compositor events.
fn sync_virtual_time(outputs: Query<&Output>, mut time: ResMut<Time<Virtual>>) {
    if outputs.is_empty() {
        time.pause();
    } else {
        time.unpause();
    }
}

pub(crate) fn pointer(
    mut messages: MessageReader<PointerInput>,
    outputs: Query<&Output>,
    regions: Query<&HitRegions>,
    mut actors: Query<(&CharacterId, &mut Config, &Transform, &AvatarState)>,
    mut drag: ResMut<Drag>,
    mut clicks: MessageWriter<Notification>,
) {
    for input in messages.read() {
        let Ok(screen) = outputs.get(input.output) else {
            continue;
        };
        let position = desktop_position(screen, input.local);
        if input.button == Some(true) {
            let hit = regions.get(input.output).ok().and_then(|regions| {
                regions
                    .0
                    .iter()
                    .filter(|(_, rect)| rect.contains(input.local))
                    .max_by_key(|(entity, _)| entity.to_bits())
                    .map(|(id, _)| *id)
            });
            if let Some(entity) = hit
                && let Ok((_, _, transform, state)) = actors.get(entity)
                && *state == AvatarState::Ready
            {
                let anchor = Vec2::new(transform.translation.x, -transform.translation.y);
                *drag = Drag {
                    entity: Some(entity),
                    start: position,
                    offset: (anchor - position) / screen.units_per_logical_pixel,
                    moved: false,
                };
            }
        }
        let Some(entity) = drag.entity else {
            continue;
        };
        drag.moved |= (position - drag.start).length() / screen.units_per_logical_pixel > 4.;
        if drag.moved {
            let destination = outputs
                .iter()
                .find(|output| bounds(output).contains(position))
                .unwrap_or(screen);
            if let Ok((_, mut placement, _, state)) = actors.get_mut(entity)
                && *state == AvatarState::Ready
            {
                placement.position = Some(
                    ((position - destination.origin) / destination.units_per_logical_pixel
                        + drag.offset)
                        .to_array(),
                );
                placement.monitor = Some(destination.name.clone());
            }
        }
        if input.button == Some(false) {
            if !drag.moved
                && let Ok((id, _, _, state)) = actors.get(entity)
                && *state == AvatarState::Ready
            {
                clicks.write(Notification(json!({"event":"click", "character":id.0})));
            }
            drag.entity = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::character::AvatarState;

    #[test]
    fn low_fps_preserves_animation_time_and_completion() {
        use bevy::ecs::message::MessageCursor;
        use std::time::Duration;

        for (fps, frame) in (1..=3)
            .map(|fps| (fps, Duration::from_secs_f64(1. / f64::from(fps))))
            .chain([(1, Duration::from_millis(1100))])
        {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, setup))
                .init_resource::<Assets<AnimationClip>>()
                .init_resource::<Assets<AnimationGraph>>()
                .add_message::<Notification>()
                .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(frame))
                .add_systems(
                    PostUpdate,
                    (
                        bevy::animation::advance_animations,
                        crate::character::notify_finished,
                    )
                        .chain(),
                );
            app.world_mut().spawn(Output {
                name: "m".into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            });
            let mut clip = AnimationClip::default();
            let elapsed = frame.as_secs_f32() * fps as f32;
            clip.set_duration(elapsed * 2.);
            let clip = app
                .world_mut()
                .resource_mut::<Assets<AnimationClip>>()
                .add(clip);
            let (graph, node) = AnimationGraph::from_clip(clip);
            let graph = app
                .world_mut()
                .resource_mut::<Assets<AnimationGraph>>()
                .add(graph);
            let mut player = AnimationPlayer::default();
            player.play(node);
            let avatar = app
                .world_mut()
                .spawn((
                    CharacterId("a".into()),
                    AvatarState::Ready,
                    Config {
                        looping: false,
                        ..default()
                    },
                    player,
                    AnimationGraphHandle(graph),
                ))
                .id();
            let mut finished = MessageCursor::<Notification>::default();
            app.update();
            for _ in 0..fps {
                app.update();
            }
            let animation = app
                .world()
                .get::<AnimationPlayer>(avatar)
                .unwrap()
                .animation(node)
                .unwrap();
            assert!(
                (animation.seek_time() - elapsed).abs() < 0.0001,
                "{fps} FPS changed playback speed"
            );
            assert_eq!(
                finished
                    .read(app.world().resource::<Messages<Notification>>())
                    .count(),
                0
            );
            for _ in 0..=fps {
                app.update();
            }
            let events: Vec<_> = finished
                .read(app.world().resource::<Messages<Notification>>())
                .collect();
            assert_eq!(events.len(), 1, "{fps} FPS missed completion");
            assert_eq!(events[0].0, json!({"event":"finished", "character":"a"}));
        }
    }

    #[derive(Resource, Default)]
    struct RegionWrites(u32);

    #[derive(Resource, Default)]
    struct TransformWrites(u32);

    fn count_region_writes(
        mut writes: ResMut<RegionWrites>,
        regions: Query<(), Changed<HitRegions>>,
    ) {
        writes.0 += regions.iter().count() as u32;
    }

    fn count_transform_writes(
        mut writes: ResMut<TransformWrites>,
        roots: Query<(), (With<CharacterId>, Changed<Transform>)>,
    ) {
        writes.0 += roots.iter().count() as u32;
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin))
            .init_resource::<RegionWrites>()
            .init_resource::<TransformWrites>()
            .add_systems(Update, (place_avatars, count_transform_writes).chain())
            .add_systems(
                PostUpdate,
                regions.after(bevy::transform::TransformSystems::Propagate),
            )
            .add_systems(PostUpdate, count_region_writes.after(regions));
        app
    }

    #[test]
    fn placement_resolves_defaults_and_stops_rewriting_when_idle() {
        let mut app = test_app();
        let output = app
            .world_mut()
            .spawn((
                Output {
                    name: "m".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                },
                Camera::default(),
            ))
            .id();
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                AvatarState::Ready,
                Config {
                    playing: false,
                    looping: true,
                    ..default()
                },
            ))
            .id();
        let mesh = app
            .world_mut()
            .spawn((
                Mesh3d(Handle::default()),
                Aabb::from_min_max(Vec3::ZERO, Vec3::splat(0.1)),
                Transform::default(),
                InheritedVisibility::VISIBLE,
                ChildOf(avatar),
            ))
            .id();

        app.update();
        let placement = app.world().get::<Config>(avatar).unwrap();
        assert_eq!(placement.monitor, None);
        assert_eq!(placement.position, None);
        let transform = *app.world().get::<Transform>(avatar).unwrap();
        assert_eq!(transform.translation, Vec3::new(50., -80., 0.));
        assert_eq!(transform.scale.x, 160.);
        assert!(!app.world().get::<HitRegions>(output).unwrap().0.is_empty());

        let before = app.world().resource::<RegionWrites>().0;
        let transform_writes = app.world().resource::<TransformWrites>().0;
        app.update();
        app.update();
        let idle = app.world().resource::<RegionWrites>().0;
        assert_eq!(before, idle, "regions rebuilt while nothing changed");
        assert_eq!(
            transform_writes,
            app.world().resource::<TransformWrites>().0,
            "placement rewrote Transform while nothing changed"
        );

        app.world_mut().get_mut::<Config>(avatar).unwrap().position = Some([20., 20.]);
        app.update();
        let dragged = app.world().resource::<RegionWrites>().0;
        assert!(dragged > idle, "regions did not follow a paused drag");
        assert!(
            app.world().resource::<TransformWrites>().0 > transform_writes,
            "placement did not follow the dragged Config"
        );

        app.world_mut().get_mut::<Config>(avatar).unwrap().playing = true;
        app.update();
        let played = app.world().resource::<RegionWrites>().0;
        assert_eq!(played, dragged, "a playing flag is not a geometry change");
        app.world_mut()
            .get_mut::<Transform>(mesh)
            .unwrap()
            .translation
            .x += 0.03;
        app.update();
        assert!(
            app.world().resource::<RegionWrites>().0 > played,
            "regions did not follow the actual mesh transform"
        );
        {
            let mut monitor = app.world_mut().get_mut::<Output>(output).unwrap();
            monitor.origin.x = -100.;
            monitor.units_per_logical_pixel = 2.;
        }
        app.update();
        let transform = app.world().get::<Transform>(avatar).unwrap();
        assert_eq!(transform.translation, Vec3::new(-60., -40., 0.));
        assert_eq!(transform.scale.x, 320.);
        app.world_mut().entity_mut(avatar).despawn();
        app.update();
        assert!(app.world().get::<HitRegions>(output).unwrap().0.is_empty());
        assert!(app.world().get::<Camera>(output).unwrap().is_active);
        app.update();
        assert!(!app.world().get::<Camera>(output).unwrap().is_active);
    }

    #[test]
    fn overlapping_avatars_pick_the_rendered_foreground_without_sorted_regions() {
        let mut app = test_app();
        app.init_resource::<Drag>()
            .add_message::<PointerInput>()
            .add_message::<Notification>()
            .add_systems(PreUpdate, pointer);
        let output = app
            .world_mut()
            .spawn((
                Output {
                    name: "m".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                },
                Camera::default(),
            ))
            .id();
        let mut actors = Vec::new();
        for name in ["a", "b"] {
            let actor = app
                .world_mut()
                .spawn((
                    CharacterId(name.into()),
                    AvatarState::Ready,
                    Config {
                        position: Some([50., 50.]),
                        ..default()
                    },
                ))
                .id();
            app.world_mut().spawn((
                Mesh3d(Handle::default()),
                Aabb::from_min_max(Vec3::splat(-0.05), Vec3::splat(0.05)),
                Transform::default(),
                InheritedVisibility::VISIBLE,
                ChildOf(actor),
            ));
            actors.push(actor);
        }
        app.update();
        let foreground = actors
            .into_iter()
            .max_by(|a, b| {
                app.world()
                    .get::<Transform>(*a)
                    .unwrap()
                    .translation
                    .z
                    .total_cmp(&app.world().get::<Transform>(*b).unwrap().translation.z)
            })
            .unwrap();
        let expected = app
            .world()
            .get::<CharacterId>(foreground)
            .unwrap()
            .0
            .clone();
        app.world_mut()
            .get_mut::<HitRegions>(output)
            .unwrap()
            .0
            .reverse();
        for button in [true, false] {
            app.world_mut().write_message(PointerInput {
                output,
                local: Vec2::splat(50.),
                button: Some(button),
            });
        }
        app.update();
        let click = app
            .world_mut()
            .resource_mut::<Messages<Notification>>()
            .drain()
            .find(|event| event.0["event"] == "click")
            .unwrap();
        assert_eq!(click.0, json!({"event":"click", "character":expected}));
    }

    #[test]
    fn removing_an_output_or_the_dragged_avatar_cancels_drag() {
        for remove_output in [false, true] {
            let mut app = App::new();
            app.add_plugins(setup);
            let avatar = app.world_mut().spawn(CharacterId("a".into())).id();
            let output = app
                .world_mut()
                .spawn(Output {
                    name: "m".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                })
                .id();
            *app.world_mut().resource_mut::<Drag>() = Drag {
                entity: Some(avatar),
                start: Vec2::ZERO,
                offset: Vec2::ZERO,
                moved: false,
            };
            app.world_mut()
                .despawn(if remove_output { output } else { avatar });
            assert!(app.world().resource::<Drag>().entity.is_none());
        }
    }

    #[test]
    fn stacking_depth_follows_membership_without_rewriting_idle_transforms() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<TransformWrites>()
            .add_systems(Update, (place_avatars, count_transform_writes).chain());
        let a = app.world_mut().spawn(CharacterId("a".into())).id();
        let b = app.world_mut().spawn(CharacterId("b".into())).id();
        app.update();
        let low = if a.to_bits() < b.to_bits() { a } else { b };
        assert_eq!(app.world().get::<Transform>(low).unwrap().translation.z, 0.);
        let writes = app.world().resource::<TransformWrites>().0;
        app.update();
        assert_eq!(app.world().resource::<TransformWrites>().0, writes);

        let c = app.world_mut().spawn(CharacterId("c".into())).id();
        app.update();
        let mut depths = [a, b, c]
            .iter()
            .map(|entity| app.world().get::<Transform>(*entity).unwrap().translation.z)
            .collect::<Vec<_>>();
        depths.sort_by(f32::total_cmp);
        assert_eq!(depths, vec![0., 1000., 2000.]);

        app.world_mut().entity_mut(b).despawn();
        app.update();
        let mut depths = [a, c]
            .iter()
            .map(|entity| app.world().get::<Transform>(*entity).unwrap().translation.z)
            .collect::<Vec<_>>();
        depths.sort_by(f32::total_cmp);
        assert_eq!(depths, vec![0., 1000.]);
    }

    #[test]
    fn cameras_follow_output_size() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_systems(Update, cameras);
        let entity = app
            .world_mut()
            .spawn(Output {
                name: "left".into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            })
            .id();
        app.world_mut().get_mut::<Output>(entity).unwrap().size = Vec2::splat(200.);
        app.update();
        let Some(Projection::Orthographic(projection)) = app.world().get::<Projection>(entity)
        else {
            panic!("camera was not spawned");
        };
        assert!(matches!(
            projection.scaling_mode,
            ScalingMode::Fixed { width, .. } if width == 200.
        ));
    }

    #[test]
    fn fallback_reports_the_selected_monitor_without_changing_the_request() {
        let output = Output {
            name: "present".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        };
        let placement = Config {
            monitor: Some("missing".into()),
            position: None,
            ..default()
        };
        let selected = pick_output(&placement, std::iter::once(&output));
        assert_eq!(selected.map(|output| output.name.as_str()), Some("present"));
        assert_eq!(
            resolved_position(&placement, selected),
            Some(Vec2::new(50., 80.))
        );
        assert_eq!(placement.monitor.as_deref(), Some("missing"));
        assert_eq!(resolved_position(&placement, None), None);
    }

    #[test]
    fn regions_observe_spring_updates_after_bounds_calculation() {
        fn spring(mut meshes: Query<&mut GlobalTransform, With<Mesh3d>>) {
            for mut transform in &mut meshes {
                *transform = GlobalTransform::from_xyz(60., -60., 0.);
            }
        }
        fn bounds(meshes: Query<&GlobalTransform, With<Mesh3d>>) {
            for transform in &meshes {
                assert_eq!(transform.translation().x, 60.);
            }
        }
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            |app: &mut App| {
                app.init_asset::<bevy::gltf::Gltf>()
                    .init_asset::<WorldAsset>()
                    .init_asset::<AnimationGraph>();
            },
            crate::character::setup,
            setup,
        ))
        .add_systems(
            PostUpdate,
            spring
                .in_set(ene_vrm::VrmSystems::SpringBone)
                .after(bevy::transform::TransformSystems::Propagate),
        )
        .add_systems(
            PostUpdate,
            bounds.in_set(VisibilitySystems::CalculateBounds),
        );
        let output = app
            .world_mut()
            .spawn((
                Output {
                    name: "m".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                },
                Camera::default(),
            ))
            .id();
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                AvatarState::Ready,
                crate::character::SceneReady,
            ))
            .id();
        app.world_mut().spawn((
            Mesh3d(Handle::default()),
            Transform::default(),
            Aabb::from_min_max(Vec3::ZERO, Vec3::splat(0.1)),
            InheritedVisibility::VISIBLE,
            ChildOf(avatar),
        ));
        app.update();
        let regions = &app.world().get::<HitRegions>(output).unwrap().0;
        assert_eq!(regions.len(), 1);
        assert!(regions[0].1.contains(Vec2::splat(60.)));
    }
}
