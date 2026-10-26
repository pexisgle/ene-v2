use super::lifecycle::{notify_finished, update_avatars};
use super::*;
use crate::desktop::Output;
use bevy::ecs::message::Messages;
use bevy::prelude::*;
use ene_vrm::{SpringJointProps, SpringRig, Vrm};
use serde_json::{Value, json};
use std::time::Duration;

fn ready() -> Vrm {
    Vrm::default()
}

fn scene_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .add_message::<Notification>()
        .add_systems(Update, update_avatars);
    app.world_mut().spawn(Output {
        name: "m".into(),
        origin: Vec2::ZERO,
        size: Vec2::splat(100.),
        units_per_logical_pixel: 1.,
    });
    app
}

fn drain_events(app: &mut App) -> Vec<Value> {
    app.world_mut()
        .resource_mut::<Messages<Notification>>()
        .drain()
        .map(|notification| notification.0)
        .collect()
}

fn spawn_output(app: &mut App) -> Entity {
    app.world_mut()
        .spawn(Output {
            name: "m".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        })
        .id()
}

#[test]
fn state_transitions() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .add_message::<Notification>()
        .add_systems(Update, update_avatars);
    let avatar = app
        .world_mut()
        .spawn((
            CharacterId("a".into()),
            Config {
                model: "a.vrm".into(),
                ..default()
            },
        ))
        .id();
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Loading)
    );
    assert_eq!(
        app.world().get::<Visibility>(avatar),
        Some(&Visibility::Hidden)
    );
    assert!(drain_events(&mut app).is_empty());

    let output = spawn_output(&mut app);
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Loading)
    );
    assert!(drain_events(&mut app).is_empty());

    app.world_mut().entity_mut(avatar).insert(ready());
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Ready)
    );
    assert_eq!(
        app.world().get::<Visibility>(avatar),
        Some(&Visibility::Inherited)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"ready","error":null})]
    );

    app.world_mut().despawn(output);
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Suspended)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"suspended","error":null})]
    );

    app.world_mut()
        .entity_mut(avatar)
        .insert(AvatarState::Error("broken".into()));
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Error("broken".into()))
    );
    assert_eq!(
        app.world().get::<Visibility>(avatar),
        Some(&Visibility::Hidden)
    );
    assert!(drain_events(&mut app).is_empty());
    app.update();
    assert!(drain_events(&mut app).is_empty());

    spawn_output(&mut app);
    app.update();
    assert!(matches!(
        app.world().get::<AvatarState>(avatar),
        Some(AvatarState::Error(_))
    ));
    assert!(drain_events(&mut app).is_empty());
}

#[test]
fn pause_and_late_parts() {
    let mut app = scene_app();
    let props = SpringJointProps {
        gravity_power: 1.,
        stiffness: 0.5,
        hit_radius: 0.02,
        ..default()
    };
    let mut actors = Vec::new();
    for playing in [false, true] {
        let root = app
            .world_mut()
            .spawn((
                CharacterId(playing.to_string()),
                AvatarState::Ready,
                ready(),
                SpringRig::default(),
                Config {
                    playing,
                    looping: true,
                    ..default()
                },
            ))
            .id();
        let mut players = Vec::new();
        for _ in 0..2 {
            let mut player = AnimationPlayer::default();
            player
                .play(AnimationNodeIndex::new(0))
                .repeat()
                .seek_to(1.25);
            players.push(app.world_mut().spawn((player, ChildOf(root))).id());
        }
        let spring = app.world_mut().spawn((props, ChildOf(root))).id();
        actors.push((root, players, spring));
    }

    app.update();
    for (index, (root, players, _)) in actors.iter().enumerate() {
        for &player in players {
            let player = app.world().get::<AnimationPlayer>(player).unwrap();
            assert_eq!(player.all_paused(), index == 0);
            assert_eq!(
                player.playing_animations().next().unwrap().1.seek_time(),
                1.25
            );
        }
        assert_eq!(
            app.world().get::<SpringRig>(*root).unwrap().paused,
            index == 0
        );
    }

    app.world_mut()
        .get_mut::<Config>(actors[0].0)
        .unwrap()
        .playing = true;
    app.update();
    assert!(
        !app.world()
            .get::<AnimationPlayer>(actors[0].1[0])
            .unwrap()
            .all_paused()
    );
    assert!(!app.world().get::<SpringRig>(actors[0].0).unwrap().paused);

    app.world_mut()
        .get_mut::<Config>(actors[0].0)
        .unwrap()
        .playing = false;
    app.update();
    let mut late = AnimationPlayer::default();
    late.play(AnimationNodeIndex::new(0)).repeat().seek_to(0.5);
    let late = app.world_mut().spawn((late, ChildOf(actors[0].0))).id();
    let late_rig = app
        .world_mut()
        .spawn((SpringRig::default(), ChildOf(actors[0].0)))
        .id();
    app.update();
    assert!(
        app.world()
            .get::<AnimationPlayer>(late)
            .unwrap()
            .all_paused()
    );
    assert!(app.world().get::<SpringRig>(late_rig).unwrap().paused);

    app.world_mut()
        .entity_mut(late)
        .insert(ChildOf(actors[1].0));
    app.world_mut()
        .entity_mut(late_rig)
        .insert(ChildOf(actors[1].0));
    app.update();
    assert!(
        !app.world()
            .get::<AnimationPlayer>(late)
            .unwrap()
            .all_paused()
    );
    assert!(!app.world().get::<SpringRig>(late_rig).unwrap().paused);
    for (_, _, spring) in actors {
        assert_eq!(app.world().get::<SpringJointProps>(spring), Some(&props));
    }
}

#[test]
fn finished_and_looping() {
    fn step(app: &mut App, frames: usize) -> Vec<String> {
        let mut events = Vec::new();
        for _ in 0..frames {
            app.update();
            events.extend(
                drain_events(app)
                    .into_iter()
                    .filter(|event| event["event"] == "finished")
                    .map(|event| event["character"].as_str().unwrap().to_owned()),
            );
        }
        events
    }

    let mut app = scene_app();
    app.init_resource::<Assets<AnimationClip>>()
        .init_resource::<Assets<AnimationGraph>>()
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(250),
        ))
        .add_systems(
            PostUpdate,
            (bevy::animation::advance_animations, notify_finished).chain(),
        );
    let mut clip = AnimationClip::default();
    clip.set_duration(1.);
    let clip = app
        .world_mut()
        .resource_mut::<Assets<AnimationClip>>()
        .add(clip);
    let (graph, node) = AnimationGraph::from_clip(clip);
    let graph = app
        .world_mut()
        .resource_mut::<Assets<AnimationGraph>>()
        .add(graph);
    let avatar = app
        .world_mut()
        .spawn((
            CharacterId("a".into()),
            AvatarState::Ready,
            ready(),
            Config {
                playing: true,
                looping: false,
                motion: Some("test.vrma".into()),
                ..default()
            },
        ))
        .id();
    let other = app
        .world_mut()
        .spawn((CharacterId("b".into()), AvatarState::Ready, ready()))
        .id();
    for (index, root) in [avatar, avatar, other].into_iter().enumerate() {
        let mut player = AnimationPlayer::default();
        player
            .play(node)
            .set_speed(if index == 1 { 0.5 } else { 1. });
        app.world_mut()
            .spawn((player, AnimationGraphHandle(graph.clone()), ChildOf(root)));
    }
    app.world_mut()
        .spawn((AnimationPlayer::default(), ChildOf(avatar)));

    assert!(step(&mut app, 1).is_empty());
    assert!(step(&mut app, 4).is_empty());
    assert_eq!(step(&mut app, 4), ["a"]);
    assert!(step(&mut app, 4).is_empty());
    app.world_mut().get_mut::<Config>(avatar).unwrap().looping = true;
    assert!(step(&mut app, 8).is_empty());
    app.world_mut().get_mut::<Config>(avatar).unwrap().looping = false;
    assert_eq!(step(&mut app, 8), ["a"]);
    assert!(step(&mut app, 4).is_empty());
}
