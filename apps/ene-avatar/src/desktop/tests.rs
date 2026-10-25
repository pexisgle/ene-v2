use super::*;
use super::{camera::cameras, hit_regions::regions, placement::place_avatars, pointer::pointer};
use crate::character::{AvatarState, CharacterId, Config, Notification};
use bevy::camera::{ScalingMode, primitives::Aabb, visibility::VisibilitySystems};
use serde_json::json;

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

fn count_region_writes(mut writes: ResMut<RegionWrites>, regions: Query<(), Changed<HitRegions>>) {
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
    let Some(Projection::Orthographic(projection)) = app.world().get::<Projection>(entity) else {
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
            ene_vrm::Vrm::default(),
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

#[test]
fn desktop_coordinates_use_monitor_origin_and_dpi() {
    let output = Output {
        name: "left".into(),
        origin: Vec2::new(-3840., 0.),
        size: Vec2::new(3840., 2160.),
        units_per_logical_pixel: 2.,
    };
    assert_eq!(
        desktop_position(&output, Vec2::new(100., 200.)),
        Vec2::new(-3640., 400.)
    );
    assert!(bounds(&output).contains(Vec2::new(-1., 100.)));
}

#[test]
fn pointer_drag_across_dpi_updates_placement_and_destination() {
    #[derive(Resource, Default)]
    struct Clicks(u32);
    fn count_clicks(mut clicks: ResMut<Clicks>, mut messages: MessageReader<Notification>) {
        clicks.0 += messages
            .read()
            .filter(|event| event.0["event"] == "click")
            .count() as u32;
    }
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<crate::desktop::PointerInput>()
        .add_message::<Notification>()
        .init_resource::<Clicks>()
        .init_resource::<Drag>()
        .add_systems(Update, (crate::desktop::pointer, count_clicks).chain());
    let actor = app
        .world_mut()
        .spawn((
            CharacterId("a".into()),
            AvatarState::Ready,
            Config {
                monitor: Some("left".into()),
                position: Some([50., 50.]),
                ..default()
            },
            Transform::from_xyz(50., -50., 0.),
        ))
        .id();
    let left = app
        .world_mut()
        .spawn((
            Output {
                name: "left".into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            },
            HitRegions(vec![(
                actor,
                Rect::from_corners(Vec2::ZERO, Vec2::splat(100.)),
            )]),
        ))
        .id();
    let right = app
        .world_mut()
        .spawn(Output {
            name: "right".into(),
            origin: Vec2::new(100., 0.),
            size: Vec2::splat(200.),
            units_per_logical_pixel: 2.,
        })
        .id();
    app.world_mut().write_message(crate::desktop::PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(true),
    });
    app.update();
    app.world_mut().write_message(crate::desktop::PointerInput {
        output: right,
        local: Vec2::new(5., 10.),
        button: None,
    });
    app.update();

    let placement = app.world().get::<Config>(actor).unwrap();
    assert_eq!(placement.monitor.as_deref(), Some("right"));
    assert_eq!(placement.position, Some([45., 40.]));
    app.world_mut().write_message(crate::desktop::PointerInput {
        output: left,
        local: Vec2::new(80., 20.),
        button: None,
    });
    app.update();
    let placement = app.world().get::<Config>(actor).unwrap();
    assert_eq!(placement.monitor.as_deref(), Some("left"));
    assert_eq!(placement.position, Some([120., 50.]));

    app.world_mut().write_message(crate::desktop::PointerInput {
        output: right,
        local: Vec2::new(5., 10.),
        button: Some(false),
    });
    app.update();
    app.world_mut().write_message(crate::desktop::PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(true),
    });
    app.update();
    app.world_mut().write_message(crate::desktop::PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(false),
    });
    app.update();
    assert_eq!(app.world().resource::<Clicks>().0, 1);
}
