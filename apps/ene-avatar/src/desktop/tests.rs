use super::*;
use super::{hit_regions::regions, placement::place_avatars, pointer::pointer};
use crate::character::{AvatarState, CharacterId, Config, ModelAssets, Notification};
use bevy::camera::primitives::Aabb;
use bevy::ecs::message::{MessageCursor, Messages};
use bevy::prelude::*;
use bevy::transform::TransformSystems;
use serde_json::{Value, json};
use std::time::Duration;

fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin))
        .add_systems(Update, place_avatars)
        .add_systems(PostUpdate, regions.after(TransformSystems::Propagate));
    app
}

fn spawn_output(app: &mut App, name: &str, size: Vec2, scale: f32) -> Entity {
    app.world_mut()
        .spawn((
            Output {
                name: name.into(),
                origin: Vec2::ZERO,
                size,
                units_per_logical_pixel: scale,
            },
            Camera::default(),
        ))
        .id()
}

fn spawn_avatar(app: &mut App, id: &str, config: Config) -> Entity {
    app.world_mut()
        .spawn((CharacterId(id.into()), AvatarState::Ready, config))
        .id()
}

fn spawn_mesh(app: &mut App, parent: Entity) -> Entity {
    app.world_mut()
        .spawn((
            Mesh3d(Handle::default()),
            Aabb::from_min_max(Vec3::ZERO, Vec3::splat(0.1)),
            Transform::default(),
            InheritedVisibility::VISIBLE,
            ChildOf(parent),
        ))
        .id()
}

fn click_events(app: &mut App) -> Vec<Value> {
    app.world_mut()
        .resource_mut::<Messages<Notification>>()
        .drain()
        .filter(|event| event.0["event"] == "click")
        .map(|event| event.0)
        .collect()
}

#[test]
fn placement_contract() {
    let mut app = test_app();
    let output = spawn_output(&mut app, "m", Vec2::splat(100.), 1.);
    let avatar = spawn_avatar(
        &mut app,
        "a",
        Config {
            playing: false,
            ..default()
        },
    );
    let mesh = spawn_mesh(&mut app, avatar);

    app.update();
    let transform = *app.world().get::<Transform>(avatar).unwrap();
    assert_eq!(transform.translation, Vec3::new(50., -80., 0.));
    assert_eq!(transform.scale, Vec3::splat(160.));
    let regions = &app.world().get::<HitRegions>(output).unwrap().0;
    assert_eq!(regions.len(), 1);
    let before = regions[0].1;

    app.world_mut().get_mut::<Config>(avatar).unwrap().position = Some([20., 20.]);
    app.update();
    let transform = *app.world().get::<Transform>(avatar).unwrap();
    assert_eq!(transform.translation, Vec3::new(20., -20., 0.));
    let after = app.world().get::<HitRegions>(output).unwrap().0[0].1;
    assert!(after.center().x < before.center().x);

    app.world_mut()
        .get_mut::<Transform>(mesh)
        .unwrap()
        .translation
        .x += 0.03;
    app.update();
    let moved = app.world().get::<HitRegions>(output).unwrap().0[0].1;
    assert!(moved.center().x > after.center().x);

    {
        let mut output = app.world_mut().get_mut::<Output>(output).unwrap();
        output.origin = Vec2::new(-100., 0.);
        output.units_per_logical_pixel = 2.;
    }
    app.update();
    let transform = *app.world().get::<Transform>(avatar).unwrap();
    assert_eq!(transform.translation, Vec3::new(-60., -40., 0.));
    assert_eq!(transform.scale, Vec3::splat(320.));

    let other = spawn_avatar(
        &mut app,
        "b",
        Config {
            position: Some([50., 50.]),
            monitor: Some("missing".into()),
            ..default()
        },
    );
    spawn_mesh(&mut app, other);
    app.update();
    let depth = |entity| app.world().get::<Transform>(entity).unwrap().translation.z;
    let mut depths = [depth(avatar), depth(other)];
    depths.sort_by(f32::total_cmp);
    assert_eq!(depths, [0., 1000.]);

    app.world_mut().get_mut::<Config>(other).unwrap().monitor = Some("missing".into());
    app.world_mut().get_mut::<Config>(other).unwrap().position = None;
    app.update();
    let transform = *app.world().get::<Transform>(other).unwrap();
    assert_eq!(transform.translation, Vec3::new(-50., -80., 0.));
    assert_eq!(transform.scale, Vec3::splat(320.));

    app.world_mut().despawn(avatar);
    app.update();
    assert_eq!(app.world().get::<HitRegions>(output).unwrap().0.len(), 1);
    app.world_mut().despawn(other);
    app.update();
    assert!(app.world().get::<HitRegions>(output).unwrap().0.is_empty());
}

#[test]
fn overlap_click_hits_front() {
    let mut app = test_app();
    app.init_resource::<Drag>()
        .add_message::<PointerInput>()
        .add_message::<Notification>()
        .add_systems(PreUpdate, pointer);
    let output = spawn_output(&mut app, "m", Vec2::splat(100.), 1.);
    let mut actors = Vec::new();
    for id in ["a", "b"] {
        let avatar = spawn_avatar(
            &mut app,
            id,
            Config {
                position: Some([50., 50.]),
                ..default()
            },
        );
        spawn_mesh(&mut app, avatar);
        actors.push((id, avatar));
    }
    app.update();

    let front = actors
        .iter()
        .max_by(|(_, left), (_, right)| {
            let depth =
                |entity: Entity| app.world().get::<Transform>(entity).unwrap().translation.z;
            depth(*left).total_cmp(&depth(*right))
        })
        .unwrap();
    for button in [Some(true), Some(false)] {
        app.world_mut().write_message(PointerInput {
            output,
            local: Vec2::splat(50.),
            button,
        });
    }
    app.update();
    assert_eq!(
        click_events(&mut app),
        [json!({"event": "click", "character": front.0})]
    );

    for button in [Some(true), Some(false)] {
        app.world_mut().write_message(PointerInput {
            output,
            local: Vec2::splat(5.),
            button,
        });
    }
    app.update();
    assert!(click_events(&mut app).is_empty());
}

#[test]
fn drag_across_monitors() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<PointerInput>()
        .add_message::<Notification>()
        .init_resource::<Drag>()
        .add_systems(Update, pointer);
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

    app.world_mut().write_message(PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(true),
    });
    app.update();
    app.world_mut().write_message(PointerInput {
        output: right,
        local: Vec2::new(5., 10.),
        button: None,
    });
    app.update();
    let placement = app.world().get::<Config>(actor).unwrap();
    assert_eq!(placement.monitor.as_deref(), Some("right"));
    assert_eq!(placement.position, Some([45., 40.]));

    app.world_mut().write_message(PointerInput {
        output: left,
        local: Vec2::new(80., 20.),
        button: None,
    });
    app.update();
    let placement = app.world().get::<Config>(actor).unwrap();
    assert_eq!(placement.monitor.as_deref(), Some("left"));
    assert_eq!(placement.position, Some([120., 50.]));

    app.world_mut().write_message(PointerInput {
        output: right,
        local: Vec2::new(5., 10.),
        button: Some(false),
    });
    app.update();
    assert!(app.world().resource::<Drag>().entity.is_none());
    assert!(click_events(&mut app).is_empty());

    app.world_mut().write_message(PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(true),
    });
    app.update();
    app.world_mut().write_message(PointerInput {
        output: left,
        local: Vec2::new(10., 20.),
        button: Some(false),
    });
    app.update();
    assert_eq!(
        click_events(&mut app),
        [json!({"event": "click", "character": "a"})]
    );
}

#[test]
fn output_removal_cancels_drag() {
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
fn low_fps_timing() {
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
            ))
            .id();
        let root = app
            .world_mut()
            .spawn((player, AnimationGraphHandle(graph), ChildOf(avatar)))
            .id();
        app.world_mut()
            .entity_mut(avatar)
            .insert(ModelAssets::with_root(root));
        let mut finished = MessageCursor::<Notification>::default();
        app.update();
        for _ in 0..fps {
            app.update();
        }
        let animation = app
            .world()
            .get::<AnimationPlayer>(root)
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
        assert_eq!(events[0].0, json!({"event": "finished", "character": "a"}));
    }
}
